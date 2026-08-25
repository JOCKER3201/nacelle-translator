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
/// rośnie bez ograniczeń, a lektor czyta treść sprzed minut zamiast bieżącej
const MAX_JOB_AGE: Duration = Duration::from_secs(18);

struct SttJob {
    id: u64,
    created: Instant,
    audio: Vec<f32>,
    coalesced: usize,
}

struct MtJob {
    id: u64,
    created: Instant,
    text: String,
    lang: String,
}

struct TtsJob {
    id: u64,
    created: Instant,
    text: String,
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
    let vad = VoiceActivityDetector::builder()
        .sample_rate(VAD_RATE as i64)
        .chunk_size(VAD_CHUNK)
        .build()?;

    let (seg_tx, seg_rx) = chan::bounded::<SttJob>(4);
    let (mt_tx, mt_rx) = chan::bounded::<MtJob>(1);
    let (tts_tx, tts_rx) = chan::bounded::<TtsJob>(1);
    let (health_tx, health_rx) = chan::bounded::<String>(4);

    let seg_cfg = cfg.vad.clone();
    let health = HealthGuard {
        name: "segmenter",
        tx: health_tx.clone(),
    };
    std::thread::Builder::new().name("segmenter".into()).spawn(move || {
        let _health = health;
        segmenter_thread(cap_cons, vad, seg_cfg, seg_tx);
    })?;

    let mt_cfg = cfg.translate.clone();
    let health = HealthGuard {
        name: "stt",
        tx: health_tx.clone(),
    };
    std::thread::Builder::new().name("stt".into()).spawn(move || {
        let _health = health;
        stt_thread(transcriber, mt_cfg, seg_rx, mt_tx);
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
        tts_thread(piper, tts_cfg, piper_bin, piper_voice, tts_rx, tts_prod);
    })?;

    Ok(health_rx)
}

fn segmenter_thread(
    mut cap_cons: HeapCons<f32>,
    mut vad: VoiceActivityDetector,
    vad_cfg: crate::config::VadCfg,
    seg_tx: chan::Sender<SttJob>,
) {
    let mut resampler = match StreamResampler::new(crate::pw::RATE as usize, VAD_RATE, 1024) {
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
            if let Some(utt) = segmenter.push_chunk(&chunk, p) {
                let id = next_id;
                next_id += 1;
                let secs = utt.audio.len() as f32 / VAD_RATE as f32;
                log::info!(
                    "#{id} segment zamknięty: {secs:.1}s{}",
                    if utt.forced { " (cięcie twarde — mowa trwa dalej)" } else { "" }
                );
                let job = SttJob {
                    id,
                    created: Instant::now(),
                    audio: utt.audio,
                    coalesced: 1,
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
    mt_cfg: crate::config::MtCfg,
    seg_rx: chan::Receiver<SttJob>,
    mt_tx: chan::Sender<MtJob>,
) {
    let mut transcriber = transcriber;
    let mut filter = HallucinationFilter::new();
    while let Ok(mut job) = seg_rx.recv() {
        // sklejamy zaległe segmenty — jeden call whispera zamiast kolejki
        while job.audio.len() < COALESCE_MAX_SAMPLES {
            match seg_rx.try_recv() {
                Ok(next) => {
                    job.audio.extend(std::iter::repeat(0.0).take(COALESCE_GAP_SAMPLES));
                    job.audio.extend_from_slice(&next.audio);
                    job.id = next.id; // raportujemy najnowszy
                    job.created = next.created;
                    job.coalesced += next.coalesced;
                }
                Err(_) => break,
            }
        }
        if job.coalesced > 1 {
            log::info!("#{} sklejono {} zaległych segmentów", job.id, job.coalesced);
        }

        let age = job.created.elapsed();
        if age > MAX_JOB_AGE {
            log::warn!(
                "#{} pominięty przed transkrypcją — zaległość {:.1}s przekracza budżet {}s",
                job.id,
                age.as_secs_f32(),
                MAX_JOB_AGE.as_secs()
            );
            continue;
        }

        let secs = job.audio.len() as f32 / VAD_RATE as f32;
        let t0 = Instant::now();
        let t = match transcriber.transcribe(&job.audio) {
            Ok(t) => t,
            Err(e) => {
                log::error!("#{} whisper: {e:#}", job.id);
                continue;
            }
        };
        let ms = t0.elapsed().as_millis();

        if let Some(reason) = filter.reject_reason(&t) {
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
            continue;
        }
        log::info!(
            "#{} 🗣 [{}] \"{}\" ({secs:.1}s audio, stt {ms} ms)",
            job.id,
            t.lang,
            t.text
        );

        if mt_cfg.skip_target_lang && t.lang == mt_cfg.target_lang_code {
            log::info!("#{} już w języku docelowym — pomijam", job.id);
            continue;
        }
        let _ = mt_tx.send(MtJob {
            id: job.id,
            created: job.created,
            text: t.text,
            lang: t.lang,
        });
    }
}

fn translate_thread(
    mut translator: Box<dyn translate::Translator>,
    mt_rx: chan::Receiver<MtJob>,
    tts_tx: chan::Sender<TtsJob>,
) {
    while let Ok(job) = mt_rx.recv() {
        let age = job.created.elapsed();
        if age > MAX_JOB_AGE {
            log::warn!(
                "#{} pominięty przed tłumaczeniem — zaległość {:.1}s przekracza budżet {}s \
                 (oszczędzam wywołanie API)",
                job.id,
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
                    job.id,
                    translated,
                    t0.elapsed().as_millis()
                );
                let _ = tts_tx.send(TtsJob {
                    id: job.id,
                    created: job.created,
                    text: translated,
                });
            }
            Err(e) => log::warn!("#{} tłumaczenie pominięte: {e:#}", job.id),
        }
    }
}

fn tts_thread(
    mut piper: PiperTts,
    cfg: crate::config::TtsCfg,
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

    while let Ok(job) = tts_rx.recv() {
        let age = job.created.elapsed();
        if age > MAX_JOB_AGE {
            log::warn!(
                "#{} pominięty przed syntezą — zaległość {:.1}s przekracza budżet {}s",
                job.id,
                age.as_secs_f32(),
                MAX_JOB_AGE.as_secs()
            );
            continue;
        }

        let t0 = Instant::now();
        let clip = match piper.synthesize(&job.text) {
            Ok(c) => c,
            Err(e) => {
                // Proces pipera padł (crash/OOM) — bez restartu synteza
                // byłaby martwa do końca życia programu. Jedna próba
                // ponownego uruchomienia i powtórzenia tego zadania.
                log::error!("#{} piper: {e:#} — próbuję zrestartować proces", job.id);
                match PiperTts::spawn(&cfg, &piper_bin, &piper_voice) {
                    Ok(fresh) => {
                        piper = fresh;
                        match piper.synthesize(&job.text) {
                            Ok(c) => c,
                            Err(e2) => {
                                log::error!("#{} piper po restarcie nadal błąd: {e2:#} — pomijam", job.id);
                                continue;
                            }
                        }
                    }
                    Err(spawn_err) => {
                        log::error!("nie mogę zrestartować pipera: {spawn_err:#} — pomijam #{}", job.id);
                        std::thread::sleep(Duration::from_secs(3));
                        continue;
                    }
                }
            }
        };
        let clip48 = match upsampler.resample(&clip) {
            Ok(c) => c,
            Err(e) => {
                log::error!("#{} resampling TTS: {e:#}", job.id);
                continue;
            }
        };
        log::info!(
            "#{} 🔊 {:.1}s mowy lektora (tts {} ms)",
            job.id,
            clip48.len() as f32 / crate::pw::RATE as f32,
            t0.elapsed().as_millis()
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
