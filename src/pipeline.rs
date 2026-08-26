//! Wątki toru AI: segmenter (VAD) → whisper → Claude → piper.
//!
//! Backpressure: kanały są celowo wąskie i każde zadanie niesie znacznik
//! czasu — gdy tor nie nadąża, zadania starsze niż `MAX_JOB_AGE` są
//! porzucane (z logiem luki) zamiast pozwalać opóźnieniu rosnąć bez końca
//! i czytać lektorem treść sprzed kilku minut.
//!
//! Nadzór: każdy wątek trzyma `HealthGuard`, który przy zakończeniu (także
//! przy panice — Rust domyślnie odwija stos) wysyła powiadomienie do
//! `run_graph`, żeby cały proces zakończył się głośno zamiast działać dalej
//! jako cichy, nieme urządzenie.

use crate::audio::{ClipResampler, StreamResampler};
use crate::config::Config;
use crate::stt::{HallucinationFilter, Transcriber};
use crate::translate;
use crate::tts::PiperTts;
use crate::vad::{Segmenter, VAD_CHUNK, VAD_RATE};
use anyhow::Result;
use crossbeam_channel as chan;
use ringbuf::{traits::*, HeapCons, HeapProd};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use voice_activity_detector::VoiceActivityDetector;

/// maksymalna łączna długość sklejonych segmentów w jednym wywołaniu whispera
const COALESCE_MAX_SAMPLES: usize = 25 * VAD_RATE;
/// cisza wstawiana między sklejane segmenty
const COALESCE_GAP_SAMPLES: usize = VAD_RATE * 3 / 10; // 0.3 s
/// gdy audio z sinka nie napływa (pauza/koniec odtwarzania) tyle czasu,
/// a segmenter ma otwartą wypowiedź, wstrzykujemy ciszę, żeby hangover mógł
/// ją domknąć — inaczej wisi w nieskończoność i skleja się z następną mową
const SILENCE_INJECT_AFTER: Duration = Duration::from_millis(200);
/// zadanie starsze niż to (liczone od zamknięcia segmentu przez VAD) jest
/// porzucane zamiast płynąć dalej przez STT/MT/TTS — inaczej opóźnienie
/// rośnie bez ograniczeń, a lektor czyta treść sprzed minut zamiast bieżącej.
/// Krótki budżet = zachowanie tłumacza symultanicznego: przy chwilowym
/// przeciążeniu gubimy fragment i wracamy do NA ŻYWO, zamiast wiernie
/// odczytywać rosnącą zaległość
const MAX_JOB_AGE: Duration = Duration::from_secs(8);

struct SttJob {
    id: u64,
    created: Instant,
    audio: Vec<f32>,
    coalesced: usize,
    /// segment ucięty w trakcie mowy (dołek/hard-max) — następny segment
    /// zaczyna się nakładką powielającą koniec tego; podstawa deduplikacji
    /// szwu w stt_thread
    forced: bool,
    /// generacja segmentera (vad.rs) — spina final z migawkami spekulacyjnego
    /// STT pobranymi z tego samego bufora audio; po koalescencji NAJNOWSZA
    /// generacja sklejki
    gen: u64,
    /// NAJSTARSZA generacja sklejki (== gen bez koalescencji) — tracker
    /// commitów mógł zatrzymać się na starszej gen (finale mają priorytet,
    /// więc pod zaległością partiale nowszej gen się nie przetwarzają);
    /// zakres [gen_oldest, gen] mówi mu, że commity dotyczą TEGO audio
    /// i deduplikują się kotwicą zamiast re-emisji hurtem
    gen_oldest: u64,
}

/// Migawka OTWARTEGO segmentu do przebiegu częściowego whispera
/// (spekulacyjne STT).
struct PartialJob {
    gen: u64,
    /// chwila pobrania migawki — diagnostyka opóźnienia kanał+whisper
    created: Instant,
    audio: Vec<f32>,
    speech_ms: u32,
}

/// Fragment = commit spekulacyjny (obietnica dostarczenia — zwolniony
/// z bramek MAX_JOB_AGE: porzucenie po commicie byłoby ubytkiem treści,
/// bo final ogona już nie powtórzy); FinalTail = ścieżka wsadowa / ogon
/// finalu (bramki bez zmian).
#[derive(Clone, Copy)]
enum JobKind {
    Fragment { gen: u64, idx: u32 },
    FinalTail,
}

impl JobKind {
    /// etykieta do logów: fragment "GEN.IDX", final — id segmentu
    fn label(&self, id: u64) -> String {
        match self {
            JobKind::Fragment { gen, idx } => format!("{gen}.{idx}"),
            JobKind::FinalTail => id.to_string(),
        }
    }
}

struct MtJob {
    id: u64,
    created: Instant,
    text: String,
    lang: String,
    /// długość oryginalnego audio segmentu [s] — do wykrywania rozdętych
    /// tłumaczeń w tts_thread; dla fragmentu szacunek: udział znakowy
    /// fragmentu w hipotezie × sekundy bufora migawki
    orig_secs: f32,
    kind: JobKind,
}

struct TtsJob {
    id: u64,
    created: Instant,
    text: String,
    orig_secs: f32,
    kind: JobKind,
}

/// Wysyła nazwę wątku przy zakończeniu (Drop biegnie i po zwykłym return,
/// i przy odwijaniu stosu po panice) — jedyny sygnał dla run_graph, że tor
/// AI przestał działać.
struct HealthGuard {
    name: &'static str,
    tx: chan::Sender<String>,
}

impl Drop for HealthGuard {
    fn drop(&mut self) {
        let _ = self.tx.send(self.name.to_string());
    }
}

/// Buduje wszystkie komponenty (fail-fast przed startem grafu), odpala
/// wątki i zwraca kanał, na który trafia nazwa wątku, jeśli któryś umrze.
pub fn spawn(
    cfg: Config,
    cap_cons: HeapCons<f32>,
    tts_prod: HeapProd<f32>,
) -> Result<chan::Receiver<String>> {
    let transcriber = Transcriber::new(&cfg.stt_model(), cfg.stt.threads, &cfg.stt.language)?;
    let translator = translate::make_translator(&cfg.translate)?;
    let piper = PiperTts::spawn(&cfg.tts, &cfg.piper_bin(), &cfg.piper_voice())?;
    // drugi piper w tempie doganiania — patrz tts_thread; oba procesy
    // startują tutaj (fail-fast), synteza i tak jest szeregowa w jednym wątku
    let mut fast_cfg = cfg.tts.clone();
    fast_cfg.length_scale = cfg.tts.catchup_length_scale;
    let piper_fast = PiperTts::spawn(&fast_cfg, &cfg.piper_bin(), &cfg.piper_voice())?;
    let vad = VoiceActivityDetector::builder()
        .sample_rate(VAD_RATE as i64)
        .chunk_size(VAD_CHUNK)
        .build()?;

    let (seg_tx, seg_rx) = chan::bounded::<SttJob>(4);
    // M6: kanał migawek bounded(1) + try_send — pełny kanał (STT zajęty)
    // oznacza naturalne opuszczenie przebiegu, nie kolejkowanie zaległości
    let (part_tx, part_rx) = chan::bounded::<PartialJob>(1);
    let (mt_tx, mt_rx) = chan::bounded::<MtJob>(1);
    let (tts_tx, tts_rx) = chan::bounded::<TtsJob>(1);
    let (health_tx, health_rx) = chan::bounded::<String>(4);

    let seg_cfg = cfg.vad.clone();
    let seg_stt_cfg = cfg.stt.clone();
    let health = HealthGuard {
        name: "segmenter",
        tx: health_tx.clone(),
    };
    std::thread::Builder::new().name("segmenter".into()).spawn(move || {
        let _health = health;
        segmenter_thread(cap_cons, vad, seg_cfg, seg_stt_cfg, seg_tx, part_tx);
    })?;

    let stt_cfg = cfg.stt.clone();
    let mt_cfg = cfg.translate.clone();
    let health = HealthGuard {
        name: "stt",
        tx: health_tx.clone(),
    };
    std::thread::Builder::new().name("stt".into()).spawn(move || {
        let _health = health;
        stt_thread(transcriber, stt_cfg, mt_cfg, seg_rx, part_rx, mt_tx);
    })?;

    let health = HealthGuard {
        name: "translate",
        tx: health_tx.clone(),
    };
    std::thread::Builder::new().name("translate".into()).spawn(move || {
        let _health = health;
        translate_thread(translator, mt_rx, tts_tx);
    })?;

    let tts_cfg = cfg.tts.clone();
    let piper_bin = cfg.piper_bin();
    let piper_voice = cfg.piper_voice();
    let health = HealthGuard {
        name: "tts",
        tx: health_tx,
    };
    std::thread::Builder::new().name("tts".into()).spawn(move || {
        let _health = health;
        tts_thread(
            piper, piper_fast, tts_cfg, fast_cfg, piper_bin, piper_voice, tts_rx, tts_prod,
        );
    })?;

    Ok(health_rx)
}

fn segmenter_thread(
    mut cap_cons: HeapCons<f32>,
    mut vad: VoiceActivityDetector,
    vad_cfg: crate::config::VadCfg,
    stt_cfg: crate::config::SttCfg,
    seg_tx: chan::Sender<SttJob>,
    part_tx: chan::Sender<PartialJob>,
) {
    /// długość chunka segmentera w ms — jednostka kadencji spekulacji
    const CHUNK_MS: u32 = (VAD_CHUNK * 1000 / VAD_RATE) as u32; // 32 ms
    // chunk 256 zamiast 1024: batching wejścia 5 ms zamiast 21 ms — mniejsze
    // stałe opóźnienie frontu toru analizy przy pomijalnym koszcie CPU
    let mut resampler = match StreamResampler::new(crate::pw::RATE as usize, VAD_RATE, 256) {
        Ok(r) => r,
        Err(e) => {
            log::error!("segmenter: nie mogę utworzyć resamplera: {e:#}");
            return;
        }
    };
    let mut segmenter = Segmenter::new(vad_cfg);
    let mut pcm16k: Vec<f32> = Vec::with_capacity(4 * VAD_RATE);
    let mut chunk = vec![0.0f32; VAD_CHUNK];
    let mut next_id: u64 = 1;
    let mut dropped: u64 = 0;
    // kadencja spekulacji: akumulator ms czasu audio od ostatniej migawki
    let mut cadence_acc: u32 = 0;
    let mut part_dropped: u64 = 0;

    loop {
        // dokładnie tyle próbek 48 kHz, ile chce resampler
        let need = resampler.need();
        {
            let buf = resampler.input_buf();
            let mut got = 0usize;
            let mut starved_for = Duration::ZERO;
            while got < need {
                let n = cap_cons.pop_slice(&mut buf[got..need]);
                got += n;
                if n == 0 {
                    // Sink nie produkuje (aplikacja w pauzie / nic nie gra).
                    // Gdy segmenter ma otwartą wypowiedź, bez nowych próbek
                    // (nawet ciszy) VAD nigdy nie zobaczy hangoveru i cała
                    // końcówka mowy wisi do wznowienia odtwarzania, gdzie
                    // skleiłaby się z kolejną wypowiedzią. Po progu czasu
                    // dopełniamy ciszą — hangover domknie segment naturalną
                    // ścieżką.
                    if starved_for >= SILENCE_INJECT_AFTER && !segmenter.is_idle() {
                        buf[got..need].fill(0.0);
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                    starved_for += Duration::from_millis(5);
                } else {
                    starved_for = Duration::ZERO;
                }
            }
        }
        match resampler.process() {
            Ok(out) => pcm16k.extend_from_slice(out),
            Err(e) => {
                log::error!("segmenter: resampling: {e:#}");
                continue;
            }
        }

        while pcm16k.len() >= VAD_CHUNK {
            chunk.copy_from_slice(&pcm16k[..VAD_CHUNK]);
            pcm16k.drain(..VAD_CHUNK);
            let p = vad.predict(chunk.iter().copied());
            let was_idle = segmenter.is_idle();
            let closed = segmenter.push_chunk(&chunk, p);
            // otwarcie wypowiedzi — bez tego log nie pokazuje fałszywych
            // wyzwoleń VAD na muzyce w scenach bez dialogu
            if was_idle && !segmenter.is_idle() {
                log::info!("#{next_id} start mowy (p={p:.2})");
            }
            // kadencja spekulacji w CZASIE AUDIO: chunk segmentera = 32 ms;
            // zegar ścienny kłamie przy wstrzykiwanej ciszy i zaległościach
            // resamplera
            if closed.is_some() || segmenter.is_idle() {
                cadence_acc = 0;
            } else if stt_cfg.speculative {
                cadence_acc += CHUNK_MS;
                if cadence_acc >= stt_cfg.cadence_ms {
                    cadence_acc = 0;
                    if let Some((gen, audio, speech_ms)) = segmenter.open_snapshot() {
                        // pad zerowy whispera (MIN_SAMPLES) daje na krótkim
                        // buforze skorelowane halucynacje — poniżej
                        // min_open_ms nie spekulujemy
                        if audio.len() >= stt_cfg.min_open_ms as usize * VAD_RATE / 1000 {
                            let pjob = PartialJob {
                                gen,
                                created: Instant::now(),
                                audio,
                                speech_ms,
                            };
                            if part_tx.try_send(pjob).is_err() {
                                // kanał bounded(1) pełny = STT zajęty —
                                // naturalne opuszczenie przebiegu (M6),
                                // nie błąd
                                part_dropped += 1;
                                if part_dropped % 30 == 0 {
                                    log::info!(
                                        "spekulacja: opuszczono {part_dropped} migawek \
                                         (STT nie nadąża za kadencją)"
                                    );
                                }
                            }
                        }
                    }
                }
            }
            if let Some(utt) = closed {
                let id = next_id;
                next_id += 1;
                let secs = utt.audio.len() as f32 / VAD_RATE as f32;
                // p_cut = p w punkcie cięcia; "-" dla domknięć nie-wymuszonych,
                // które nie mają punktu cięcia. To jedyna liczba mówiąca, jak
                // głęboki był dołek, w którym faktycznie przecięliśmy bufor —
                // pmin obejmuje CAŁY segment (także czas sprzed okna śledzenia
                // dołków), więc nie odpowiada na to pytanie. p_n to liczba
                // chunków objętych statystyką (32 ms każdy) — po cięciu
                // wymuszonym liczniki startują od zera, więc p̄ nie opisuje
                // całego bufora następnego segmentu.
                let p_cut = match utt.p_dip {
                    Some(p) => format!("{p:.2}"),
                    None => "-".to_string(),
                };
                log::info!(
                    "#{id} segment zamknięty: {secs:.1}s (powód: {}, p̄={:.2}, pmin={:.2}, \
                     p_cut={p_cut}, p_n={}){}",
                    utt.reason.label(),
                    utt.p_mean,
                    utt.p_min,
                    utt.p_n,
                    if utt.forced { " — mowa trwa dalej" } else { "" }
                );
                let job = SttJob {
                    id,
                    created: Instant::now(),
                    audio: utt.audio,
                    coalesced: 1,
                    forced: utt.forced,
                    gen: utt.gen,
                    gen_oldest: utt.gen,
                };
                if seg_tx.try_send(job).is_err() {
                    dropped += 1;
                    log::warn!(
                        "#{id} kolejka STT pełna — segment odrzucony (łącznie: {dropped})"
                    );
                }
            }
        }
    }
}

fn stt_thread(
    transcriber: Transcriber,
    stt_cfg: crate::config::SttCfg,
    mt_cfg: crate::config::MtCfg,
    seg_rx: chan::Receiver<SttJob>,
    part_rx: chan::Receiver<PartialJob>,
    mt_tx: chan::Sender<MtJob>,
) {
    /// praca do wykonania w jednym obiegu pętli: final ma zawsze priorytet
    enum Work {
        Final(SttJob),
        Partial(PartialJob),
    }
    let mut transcriber = transcriber;
    let mut filter = HallucinationFilter::new();
    let mut tracker = crate::agreement::SpeculativeTracker::new(stt_cfg.min_fragment_chars);
    // licznik migawek odrzuconych jako przestarzała generacja (M1)
    let mut stale_dropped: u64 = 0;
    // generacja, dla której już zalogowano wstrzymanie fragmentów w języku
    // docelowym — jeden log na segment zamiast spamu co kadencję
    let mut skip_logged: Option<u64> = None;
    // ostatni ZAAKCEPTOWANY segment: (id, znormalizowane ostatnie słowo,
    // czy był ucięty w trakcie mowy i niesklejony) — do deduplikacji szwu
    let mut prev_seam: Option<(u64, String, bool)> = None;
    // segment, który nie zmieścił się w limicie sklejania — przetwarzany
    // w następnym obiegu zamiast przepaść (try_recv już zdjął go z kanału)
    let mut pending: Option<SttJob> = None;
    loop {
        let work = if let Some(j) = pending.take() {
            Work::Final(j)
        } else if !stt_cfg.speculative {
            // ścieżka wsadowa 1:1 jak dotąd — zero zmian zachowania
            match seg_rx.recv() {
                Ok(j) => Work::Final(j),
                Err(_) => break,
            }
        } else if let Ok(j) = seg_rx.try_recv() {
            // M6: PRIORYTET finalów — final czekający w kolejce wygrywa
            // z każdą migawką (select niżej nie gwarantuje kolejności)
            Work::Final(j)
        } else {
            chan::select! {
                recv(seg_rx) -> r => match r {
                    Ok(j) => Work::Final(j),
                    Err(_) => break,
                },
                recv(part_rx) -> r => match r {
                    Ok(p) => Work::Partial(p),
                    Err(_) => break,
                },
            }
        };
        let mut job = match work {
            Work::Final(j) => j,
            Work::Partial(p) => {
                handle_partial(
                    &mut transcriber,
                    &mut tracker,
                    &stt_cfg,
                    &mt_cfg,
                    &mt_tx,
                    p,
                    &mut stale_dropped,
                    &mut skip_logged,
                );
                continue;
            }
        };
        // sklejamy zaległe segmenty — jeden call whispera zamiast kolejki.
        // Limit sprawdzany PO doklejeniu: whisper z single_segment dekoduje
        // jedno okno 30 s, więc sklejka ponad COALESCE_MAX_SAMPLES cicho
        // gubiłaby wszystko powyżej okna
        loop {
            match seg_rx.try_recv() {
                Ok(next) => {
                    if job.audio.len() + COALESCE_GAP_SAMPLES + next.audio.len()
                        > COALESCE_MAX_SAMPLES
                    {
                        pending = Some(next);
                        break;
                    }
                    job.audio.extend(std::iter::repeat(0.0).take(COALESCE_GAP_SAMPLES));
                    job.audio.extend_from_slice(&next.audio);
                    job.id = next.id; // raportujemy najnowszy
                    job.created = next.created;
                    job.coalesced += next.coalesced;
                    job.forced = next.forced;
                    // najnowsza generacja — spójne z min_gen = gen+1 po
                    // finale, bo starsze generacje też są już domknięte;
                    // gen_oldest zostaje — tracker deduplikuje commity
                    // starszej gen kotwicą po całym zakresie sklejki
                    job.gen = next.gen;
                }
                Err(_) => break,
            }
        }
        if job.coalesced > 1 {
            log::info!("#{} sklejono {} zaległych segmentów", job.id, job.coalesced);
        }

        let age = job.created.elapsed();
        // final generacji ze scommitowanymi fragmentami jest ZWOLNIONY
        // z budżetu wieku i z filtra halucynacji: commit to obietnica
        // dostarczenia — ogon domyka zdanie, którego początek lektor już
        // przeczytał, a zaległość jest częściowo samozawiniona (dodatkowe
        // przebiegi GPU + ruch MT spekulacji); koszt transkrypcji ograniczony
        let committed_final =
            stt_cfg.speculative && tracker.has_commits(job.gen_oldest, job.gen);
        if age > MAX_JOB_AGE && !committed_final {
            log::warn!(
                "#{} pominięty przed transkrypcją — zaległość {:.1}s przekracza budżet {}s",
                job.id,
                age.as_secs_f32(),
                MAX_JOB_AGE.as_secs()
            );
            // final porzucony: tracker musi domknąć generację (scommitowany
            // ogon przepada — strata liczona i logowana w trackerze)
            if stt_cfg.speculative {
                tracker.on_final_rejected(job.gen_oldest, job.gen);
            }
            continue;
        }

        let secs = job.audio.len() as f32 / VAD_RATE as f32;
        let t0 = Instant::now();
        let mut t = match transcriber.transcribe(&job.audio) {
            Ok(t) => t,
            Err(e) => {
                log::error!("#{} whisper: {e:#}", job.id);
                if stt_cfg.speculative {
                    tracker.on_final_rejected(job.gen_oldest, job.gen);
                }
                continue;
            }
        };
        let ms = t0.elapsed().as_millis();

        if let Some(reason) = filter.reject_reason(&t) {
            if committed_final && !t.text.is_empty() {
                // final z commitami: odrzucenie hurtem byłoby ubytkiem ogona
                // zdania, którego początek dwa przebiegi częściowe już
                // potwierdziły i lektor przeczytał (typowy false positive:
                // dedup last_seen na legalnie powtórzonej frazie) — ogon
                // zostanie dostarczony, powód tylko logujemy
                log::warn!(
                    "#{} filtr zgłasza \"{reason}\", ale generacja ma scommitowane \
                     fragmenty — dostarczam ogon mimo to",
                    job.id
                );
            } else {
                // warn, nie debug: bez tego odrzucenia znikały bez śladu na
                // domyślnym poziomie logu i wyglądało to, jakby program
                // "nic nie robił", mimo że whisper realnie transkrybował
                log::warn!(
                    "#{} odrzucone — {reason}: \"{}\" ({secs:.1}s audio, no_speech {:.2}, logprob {:.2})",
                    job.id,
                    t.text,
                    t.no_speech_prob,
                    t.avg_logprob
                );
                // M4: filtr działa na finalach jak dotąd (fragmenty go nie
                // widzą); odrzucony final i tak domyka generację w trackerze
                if stt_cfg.speculative {
                    tracker.on_final_rejected(job.gen_oldest, job.gen);
                }
                continue;
            }
        }
        // Deduplikacja szwu: po cięciu wymuszonym nakładka 250 ms powiela
        // ostatnie słowo poprzedniego segmentu ("...but until" → "Until we...").
        // Twarde bramki (werdykt weryfikacji floty): tylko bezpośrednio
        // kolejne id, oba segmenty niesklejone, poprzedni ucięty w trakcie
        // mowy, zdejmowane najwyżej JEDNO słowo — duplikat jest tańszy niż
        // zgubione słowo, więc przy jakiejkolwiek wątpliwości nie zdejmujemy.
        if let Some((pid, last_w, seam_ok)) = &prev_seam {
            if *seam_ok && job.id == *pid + 1 && job.coalesced == 1 {
                if let Some(first_w) = t.text.split_whitespace().next() {
                    if !last_w.is_empty() && crate::stt::normalize(first_w) == *last_w {
                        let cut_from =
                            t.text.find(first_w).unwrap_or(0) + first_w.len();
                        let rest = t.text[cut_from..].trim_start().to_string();
                        log::info!(
                            "#{} szew: zdjęto powtórzone \"{first_w}\" z nakładki",
                            job.id
                        );
                        t.text = rest;
                    }
                }
            }
        }
        if t.text.is_empty() {
            prev_seam = None;
            if stt_cfg.speculative {
                tracker.on_final_rejected(job.gen_oldest, job.gen);
            }
            continue;
        }
        prev_seam = Some((
            job.id,
            t.text
                .split_whitespace()
                .last()
                .map(crate::stt::normalize)
                .unwrap_or_default(),
            job.forced && job.coalesced == 1,
        ));
        // no_speech/logprob także dla ZAAKCEPTOWANYCH — bez tego nie da się
        // z logu ocenić, czy przepuszczone śmieci były "pewne" (filtr
        // bezradny) czy graniczne (progi do stroju)
        log::info!(
            "#{} 🗣 [{}] \"{}\" ({secs:.1}s audio, stt {ms} ms, no_speech {:.2}, logprob {:.2})",
            job.id,
            t.lang,
            t.text,
            t.no_speech_prob,
            t.avg_logprob
        );

        // M3: ogon finalu — on_final MUSI pobiec przed decyzją skip niżej,
        // bo reset generacji w trackerze jest potrzebny niezależnie od niej
        let (text, orig_secs, mt_lang, had_commits) = if stt_cfg.speculative {
            // lock trzeba odczytać PRZED on_final — reset segmentu go czyści
            let locked = tracker.locked_lang().map(str::to_string);
            let emit = tracker.on_final(job.gen, job.gen_oldest, job.coalesced, &t.text);
            if emit.reanchored {
                log::warn!(
                    "#{} re-kotwiczenie ogona finalu — emisja z duplikatem (łącznie: {})",
                    job.id,
                    tracker.reanchors
                );
            }
            if emit.text.is_empty() {
                log::info!("#{} final w całości pokryty fragmentami", job.id);
                continue;
            }
            // ogon segmentu z commitami idzie w języku ZAMROŻONYM: fragmenty
            // przetłumaczono z tego języka, a spójność wewnątrz zdania jest
            // ważniejsza niż detekcja finalu (flap detekcji częściowy/pełny
            // bufor to dokładnie to, przed czym lang_lock chroni)
            let lang = match (emit.had_commits, locked) {
                (true, Some(l)) => l,
                _ => t.lang.clone(),
            };
            (emit.text, secs * emit.char_share, lang, emit.had_commits)
        } else {
            (t.text, secs, t.lang.clone(), false)
        };

        // M5: skip_target_lang decydowany WYŁĄCZNIE na finale (fragmenty
        // w języku docelowym są wstrzymywane bez decyzji — handle_partial).
        // Wyjątek: gdy fragmenty JUŻ wyszły (lock zamroził język nie-docelowy),
        // rozjazd lock vs detekcja finalu rozstrzygamy w stronę DOSTARCZENIA
        // ogona — słuchacz usłyszał początek zdania, urwanie reszty byłoby
        // ubytkiem (zasada nadrzędna: duplikat/nadmiar, nigdy ubytek)
        if mt_cfg.skip_target_lang && t.lang == mt_cfg.target_lang_code {
            if had_commits {
                log::warn!(
                    "#{} rozjazd lang_lock ({mt_lang}) vs detekcja finalu ({}) — \
                     fragmenty już wyszły, dostarczam ogon mimo języka docelowego",
                    job.id,
                    t.lang
                );
            } else {
                log::info!("#{} już w języku docelowym — pomijam", job.id);
                continue;
            }
        }
        let _ = mt_tx.send(MtJob {
            id: job.id,
            created: job.created,
            text,
            lang: mt_lang,
            orig_secs,
            kind: JobKind::FinalTail,
        });
    }
}

/// Obsługa przebiegu częściowego (migawki otwartego segmentu) w wątku STT.
/// Fragmenty NIE przechodzą przez HallucinationFilter (M4) — bramkują je
/// wyłącznie: min długość (w trackerze) i lang_lock (M5).
#[allow(clippy::too_many_arguments)]
fn handle_partial(
    transcriber: &mut Transcriber,
    tracker: &mut crate::agreement::SpeculativeTracker,
    stt_cfg: &crate::config::SttCfg,
    mt_cfg: &crate::config::MtCfg,
    mt_tx: &chan::Sender<MtJob>,
    p: PartialJob,
    stale_dropped: &mut u64,
    skip_logged: &mut Option<u64>,
) {
    // M1: przebieg starszej generacji w całości do kosza — ZANIM zapłacimy
    // za whispera (bufor, z którego pobrano migawkę, już nie istnieje)
    if tracker.is_stale(p.gen) {
        *stale_dropped += 1;
        log::debug!(
            "#{} migawka przestarzałej generacji odrzucona (łącznie: {stale_dropped})",
            p.gen
        );
        return;
    }
    // język stały w konfiguracji = lock trywialny od pierwszego przebiegu
    if stt_cfg.language != "auto" && tracker.locked_lang().is_none() {
        tracker.lock_lang(&stt_cfg.language);
    }
    let locked = tracker.locked_lang().map(str::to_string);
    let buf_secs = p.audio.len() as f32 / VAD_RATE as f32;
    let t0 = Instant::now();
    let (t, lang_prob) = match transcriber.transcribe_partial(&p.audio, locked.as_deref()) {
        Ok(r) => r,
        Err(e) => {
            // to nie final — bez on_final_rejected; następny przebieg nadrobi
            log::warn!("#{} whisper (przebieg częściowy): {e:#}", p.gen);
            return;
        }
    };
    log::debug!(
        "#{} przebieg częściowy: {buf_secs:.1}s audio, stt {} ms, mowa {} ms, \
         migawka sprzed {} ms: \"{}\"",
        p.gen,
        t0.elapsed().as_millis(),
        p.speech_ms,
        p.created.elapsed().as_millis(),
        t.text
    );

    // M5: lang_lock dopiero przy pewnej detekcji (p >= LOCK_MIN_PROB)
    // i buforze >= LOCK_MIN_MS — krótsze audio flapuje między językami
    if tracker.locked_lang().is_none() {
        let buf_ms = (p.audio.len() * 1000 / VAD_RATE) as u32;
        if buf_ms >= crate::agreement::LOCK_MIN_MS
            && lang_prob.unwrap_or(0.0) >= crate::agreement::LOCK_MIN_PROB
            && t.lang != "und"
        {
            tracker.lock_lang(&t.lang);
            log::info!(
                "#{} lang_lock: {} (p={:.2}, bufor {buf_secs:.1}s)",
                p.gen,
                t.lang,
                lang_prob.unwrap_or(0.0)
            );
        }
    }

    // przebiegi sprzed locka TEŻ karmią tracker (audio jest append-only,
    // więc zgoda przebiegów pozostaje ważna) — bramkowana jest tylko emisja
    let pending_frag = tracker.on_partial(p.gen, &t.text);

    // M5: przed zamrożeniem języka NIE emitować
    let Some(locked) = tracker.locked_lang().map(str::to_string) else {
        return;
    };
    // M5: skip_target_lang decydowany WYŁĄCZNIE na finale — fragmenty
    // w języku docelowym po prostu nie wychodzą (committed zostaje 0,
    // więc final wyemituje całość i sam podejmie decyzję skip jak dziś)
    if mt_cfg.skip_target_lang && locked == mt_cfg.target_lang_code {
        if *skip_logged != Some(p.gen) {
            *skip_logged = Some(p.gen);
            log::info!(
                "#{} przebiegi częściowe w języku docelowym — fragmenty wstrzymane \
                 (decyzję podejmie final)",
                p.gen
            );
        }
        return;
    }

    if let Some(frag) = pending_frag {
        let n_words = frag.text.split_whitespace().count();
        let mjob = MtJob {
            id: 0, // nieużywane dla fragmentów — logi biorą kind.label()
            created: Instant::now(), // wiek fragmentu liczony OD CHWILI COMMITU
            text: frag.text.clone(),
            lang: locked,
            orig_secs: buf_secs * frag.char_share,
            kind: JobKind::Fragment {
                gen: p.gen,
                idx: frag.idx,
            },
        };
        // try_send zamiast send blokującego: pełny kanał mt (wolny translator)
        // zawieszałby wątek STT na pełną latencję tłumaczenia, finale
        // starzałyby się w seg_rx ponad MAX_JOB_AGE i spekulacja gubiłaby
        // WIĘCEJ niż ścieżka wsadowa. Porzucenie NIEscommitowanego fragmentu
        // jest legalne — tracker zaproponuje identyczny przy następnym
        // przebiegu (idempotencja: test T9). Niezmiennik M2 zachowany:
        // committed przesuwamy wyłącznie po udanym umieszczeniu w kanale.
        match mt_tx.try_send(mjob) {
            Ok(()) => {
                log::info!(
                    "#{}.{} fragment ({n_words} słów): \"{}\"",
                    p.gen,
                    frag.idx,
                    frag.text
                );
                tracker.commit(&frag);
            }
            Err(chan::TrySendError::Full(_)) => {
                log::debug!(
                    "#{}.{} kanał mt pełny — fragment nie scommitowany, wróci \
                     w następnym przebiegu",
                    p.gen,
                    frag.idx
                );
            }
            Err(chan::TrySendError::Disconnected(_)) => {}
        }
    }
}

fn translate_thread(
    mut translator: Box<dyn translate::Translator>,
    mt_rx: chan::Receiver<MtJob>,
    tts_tx: chan::Sender<TtsJob>,
) {
    // fragmenty spekulacyjne utracone na błędzie tłumaczenia — committed
    // w trackerze JUŻ przesunięte, więc treść przepada bezpowrotnie (M2:
    // strata zaakceptowana świadomie, ale liczona i logowana)
    let mut lost_fragments: u64 = 0;
    // księgowość porzuceń analogiczna do tracker.on_final_rejected w stt_thread:
    // bez licznika sumarycznego pojedyncza linia WARN nie mówi, czy to incydent,
    // czy stan trwały (w logu audytu bramka tts odpaliła raz — i nie było jak
    // tego odróżnić od serii)
    let mut dropped_finals: u64 = 0;
    while let Ok(job) = mt_rx.recv() {
        let is_fragment = matches!(job.kind, JobKind::Fragment { .. });
        let age = job.created.elapsed();
        // M2: commit to obietnica dostarczenia — fragmenty NIE podlegają
        // budżetowi porzucania (porzucenie = ubytek treści, bo final ogona
        // już nie powtórzy); ścieżka wsadowa (FinalTail) bez zmian
        if !is_fragment && age > MAX_JOB_AGE {
            dropped_finals += 1;
            log::warn!(
                "#{} pominięty przed tłumaczeniem — zaległość {:.1}s przekracza budżet {}s \
                 (oszczędzam wywołanie API) (porzuconych ogonów łącznie: {dropped_finals})",
                job.kind.label(job.id),
                age.as_secs_f32(),
                MAX_JOB_AGE.as_secs()
            );
            continue;
        }
        let t0 = Instant::now();
        match translator.translate(&job.text, &job.lang) {
            Ok(translated) => {
                log::info!(
                    "#{} 🌐 \"{}\" (mt {} ms)",
                    job.kind.label(job.id),
                    translated,
                    t0.elapsed().as_millis()
                );
                let _ = tts_tx.send(TtsJob {
                    id: job.id,
                    created: job.created,
                    text: translated,
                    orig_secs: job.orig_secs,
                    kind: job.kind,
                });
            }
            Err(e) => {
                if is_fragment {
                    lost_fragments += 1;
                    log::warn!(
                        "#{} fragment przepadł w tłumaczeniu (strat łącznie: \
                         {lost_fragments}): {e:#}",
                        job.kind.label(job.id)
                    );
                } else {
                    log::warn!(
                        "#{} tłumaczenie pominięte: {e:#}",
                        job.kind.label(job.id)
                    );
                }
            }
        }
    }
}

/// Zadanie z zaległością powyżej tego progu jest syntetyzowane w tempie
/// doganiania (catchup_length_scale) — lektor chwilowo przyspiesza, żeby
/// spłacić dług kolejki po rozwlekłym tłumaczeniu, zamiast pozwalać
/// zaległości urosnąć do budżetu porzucania (MAX_JOB_AGE).
const CATCHUP_AFTER: Duration = Duration::from_secs(1);

/// Powód, dla którego lektor odchodzi od tempa nominalnego.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tempo {
    Nominalne,
    /// zaległość zadania albo rozdęte tłumaczenie
    Doganianie,
}

/// Wydzielone z tts_thread jako funkcja czysta — wybór tempa to jedyna decyzja
/// tego wątku zależna od danych, a tak daje się sprawdzić testem bez procesów
/// pipera i bez wątków.
fn pick_tempo(age: Duration, chars: usize, orig_secs: f32) -> Tempo {
    let oversized = chars as f32 / orig_secs.max(0.5) > 30.0;
    if age > CATCHUP_AFTER || oversized {
        Tempo::Doganianie
    } else {
        Tempo::Nominalne
    }
}

/// Synteza z jedną próbą restartu procesu pipera po awarii (crash/OOM) —
/// bez restartu synteza byłaby martwa do końca życia programu.
fn synth_with_restart(
    piper: &mut PiperTts,
    cfg: &crate::config::TtsCfg,
    piper_bin: &PathBuf,
    piper_voice: &PathBuf,
    label: &str,
    text: &str,
) -> Option<Vec<f32>> {
    match piper.synthesize(text) {
        Ok(c) => Some(c),
        Err(e) => {
            log::error!("#{label} piper: {e:#} — próbuję zrestartować proces");
            match PiperTts::spawn(cfg, piper_bin, piper_voice) {
                Ok(fresh) => {
                    *piper = fresh;
                    match piper.synthesize(text) {
                        Ok(c) => Some(c),
                        Err(e2) => {
                            log::error!("#{label} piper po restarcie nadal błąd: {e2:#} — pomijam");
                            None
                        }
                    }
                }
                Err(spawn_err) => {
                    log::error!("nie mogę zrestartować pipera: {spawn_err:#} — pomijam #{label}");
                    std::thread::sleep(Duration::from_secs(3));
                    None
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn tts_thread(
    mut piper: PiperTts,
    mut piper_fast: PiperTts,
    cfg: crate::config::TtsCfg,
    fast_cfg: crate::config::TtsCfg,
    piper_bin: PathBuf,
    piper_voice: PathBuf,
    tts_rx: chan::Receiver<TtsJob>,
    mut tts_prod: HeapProd<f32>,
) {
    let mut upsampler =
        match ClipResampler::new(piper.sample_rate as usize, crate::pw::RATE as usize) {
            Ok(r) => r,
            Err(e) => {
                log::error!("tts: nie mogę utworzyć resamplera: {e:#}");
                return;
            }
        };

    // księgowość porzuceń jak w translate_thread — ostatnia bramka przed
    // głośnikiem, więc jej licznik jest najbliższym odpowiednikiem „ile treści
    // realnie nie usłyszał słuchacz"
    let mut dropped_finals: u64 = 0;

    while let Ok(job) = tts_rx.recv() {
        let label = job.kind.label(job.id);
        let age = job.created.elapsed();
        // M2: fragmenty spekulacyjne zwolnione z budżetu porzucania — commit
        // to obietnica dostarczenia; tryb doganiania niżej dalej działa
        // (przyspiesza odczyt, niczego nie porzuca)
        if !matches!(job.kind, JobKind::Fragment { .. }) && age > MAX_JOB_AGE {
            dropped_finals += 1;
            log::warn!(
                "#{label} pominięty przed syntezą — zaległość {:.1}s przekracza budżet {}s \
                 (porzuconych ogonów łącznie: {dropped_finals})",
                age.as_secs_f32(),
                MAX_JOB_AGE.as_secs()
            );
            continue;
        }

        // Rozdęte tłumaczenie (dużo więcej znaków, niż oryginał zdołałby
        // wypowiedzieć — typowy owoc halucynacji MT na zniekształconej
        // transkrypcji) idzie w tempie doganiania NIEZALEŻNIE od zaległości:
        // normalne tempo przy ~20 zn/s odtwarzania oznacza, że taki klip
        // sam z siebie tworzy kilkusekundowy dług.
        let tempo = pick_tempo(age, job.text.chars().count(), job.orig_secs);
        let behind = tempo != Tempo::Nominalne;
        let t0 = Instant::now();
        let clip = {
            let (active, active_cfg) = if behind {
                (&mut piper_fast, &fast_cfg)
            } else {
                (&mut piper, &cfg)
            };
            match synth_with_restart(active, active_cfg, &piper_bin, &piper_voice, &label, &job.text)
            {
                Some(c) => c,
                None => continue,
            }
        };
        let clip48 = match upsampler.resample(&clip) {
            Ok(c) => c,
            Err(e) => {
                log::error!("#{label} resampling TTS: {e:#}");
                continue;
            }
        };
        // Rzeczywista zaległość odsłuchu, a nie „wiek": ile mowy lektora czeka
        // JESZCZE w ringu, zanim ten klip w ogóle zacznie grać. `wiek` dla
        // fragmentów startuje dopiero przy commicie (patrz MtJob w
        // handle_partial), więc mierzy burzliwość napływu — zmierzona korelacja
        // z realną zaległością odtwarzania to 0.06, a po serii doganiania
        // `wiek` spadał 4,6 → 0,7 s, podczas gdy realna zaległość ROSŁA
        // 3,7 → 7,4 s. Odczyt indeksów atomowych ringu: bez blokowania, bez
        // alokacji. Pomiar MUSI być przed pętlą push_slice niżej — po niej ring
        // jest zawsze pełny i liczba traci sens.
        // CZYTAJĄC LOG: `ring` jest z definicji ograniczony pojemnością ringu
        // (3 s, main.rs), a pomiar wypada tuż po zakończeniu BLOKUJĄCEJ pętli
        // push_slice poprzedniego klipu, więc przy nasyconym torze będzie
        // wisiał tuż pod 3.0 s. Wartość informacyjną ma dopiero wtedy, gdy
        // WYRAŹNIE spada poniżej — to znaczy, że kolejka odtwarzania faktycznie
        // się opróżniła. Reszta zaległości siedzi w `wiek`, bo cały tor to
        // łańcuch kanałów bounded(1) z blokującym send: zaległość nie stoi
        // w kolejce, tylko w zablokowanych wątkach, i wychodzi na jaw jako
        // wiek zadania. NIE logujemy `tts_rx.len()` — kanał ma pojemność 1,
        // więc byłby to jeden bit udający głębokość kolejki.
        // WYŁĄCZNIE DIAGNOSTYKA — `ring_s` NIE wchodzi do żadnej decyzji
        // o tempie ani o porzucaniu (progi są dziś strojone na szum i najpierw
        // potrzebują danych, a nie kolejnej regulacji).
        let ring_s = tts_prod.occupied_len() as f32 / crate::pw::RATE as f32;
        let age_now = job.created.elapsed();
        // wiek = pełne opóźnienie toru od zamknięcia segmentu do gotowej
        // syntezy — jedna liczba mówiąca, czy problem leży w etapach AI
        // (wiek mały) czy w kolejce odtwarzania (wiek rośnie z segmentu
        // na segment). Nowe pola dopisane NA KOŃCU nawiasu, żeby dotychczasowe
        // skrypty czytające „wiek {x}s" dalej działały.
        log::info!(
            "#{label} 🔊 {:.1}s mowy lektora (tts {} ms, wiek {:.1}s, ring {ring_s:.1}s, \
             zaległość odsłuchu {:.1}s{})",
            clip48.len() as f32 / crate::pw::RATE as f32,
            t0.elapsed().as_millis(),
            age_now.as_secs_f32(),
            age_now.as_secs_f32() + ring_s,
            match tempo {
                Tempo::Nominalne => "",
                Tempo::Doganianie => ", tryb doganiania",
            }
        );
        // push z czekaniem — ring daje naturalny backpressure
        let mut rest = &clip48[..];
        while !rest.is_empty() {
            let n = tts_prod.push_slice(rest);
            rest = &rest[n..];
            if !rest.is_empty() {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// tempo nominalne: świeże zadanie, tłumaczenie mieszczące się w oryginale
    #[test]
    fn p1_tempo_nominalne_bez_zaleglosci() {
        let t = pick_tempo(Duration::from_millis(100), 30, 3.0);
        assert_eq!(t, Tempo::Nominalne);
    }

    #[test]
    fn p2_zaleglosc_wlacza_doganianie() {
        let t = pick_tempo(Duration::from_millis(1_500), 30, 3.0);
        assert_eq!(t, Tempo::Doganianie);
    }

    /// odtwarza obserwację audytu: 4 klipy poszły w doganianiu przy wieku
    /// 0,1-0,2 s, bo heurystyka `oversized` dzieli znaki przez ZGADYWANY
    /// orig_secs. Zachowanie celowo NIEZMIENIONE (naprawa progu to rekomendacja
    /// 8, której nie wdrażamy) — test utrwala stan, żeby przyszła zmiana progu
    /// była widoczna.
    #[test]
    fn p3_rozdete_tlumaczenie_wlacza_doganianie() {
        let t = pick_tempo(Duration::from_millis(100), 100, 1.0);
        assert_eq!(t, Tempo::Doganianie);
    }

    /// orig_secs dla fragmentu to szacunek (udział znakowy × sekundy bufora)
    /// i może wyjść zerowy — zabezpiecza istniejące `.max(0.5)`
    #[test]
    fn p6_orig_secs_zero_nie_dzieli_przez_zero() {
        let t = pick_tempo(Duration::from_millis(100), 1, 0.0);
        assert_eq!(t, Tempo::Nominalne);
        let t = pick_tempo(Duration::from_millis(100), 100, 0.0);
        assert_eq!(t, Tempo::Doganianie);
    }
}
