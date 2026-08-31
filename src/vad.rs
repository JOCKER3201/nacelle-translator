//! Segmentacja mowy: Silero VAD (voice_activity_detector) + maszyna stanów
//! z histerezą, hangoverem, pre-rollem i cięciem długiej mowy w dołku
//! prawdopodobieństwa (wzorce z obs-localvocal i stream-translator-gpt).

use crate::config::VadCfg;
use std::collections::VecDeque;

/// Silero V5 przy 16 kHz przyjmuje wyłącznie okna 512 próbek (32 ms).
pub const VAD_CHUNK: usize = 512;
pub const VAD_RATE: usize = 16_000;
const CHUNK_MS: u32 = (VAD_CHUNK * 1000 / VAD_RATE) as u32; // 32 ms

pub struct Utterance {
    /// mono f32 16 kHz
    pub audio: Vec<f32>,
    /// generacja segmentu, pod którą powstawały przebiegi częściowe
    /// spekulacyjnego STT — final i migawki z tą samą wartością należą do
    /// tego samego bufora audio
    pub gen: u64,
    /// segment ucięty w trakcie mowy (mowa trwa dalej)
    pub forced: bool,
    /// jak doszło do domknięcia — kluczowa dana diagnostyczna dla trudnego
    /// materiału (film z muzyką): dominacja hard-max przy wysokim `p_dip`
    /// oznacza, że podkład trzyma p nad progami i VAD nie ma się o co
    /// zaczepić. UWAGA: rozstrzyga o tym `p_dip`, a NIE `p_min` — patrz niżej.
    pub reason: CloseReason,
    /// najniższe p zaobserwowane w segmencie. Zakres liczenia: od PIERWSZEGO
    /// chunka MOWY bieżącego segmentu (albo od cięcia wymuszonego) po każdym
    /// kolejnym chunku aż do domknięcia. Chunki PREROLLU są w buforze audio,
    /// ale w tej statystyce ich NIE MA — `note_p` woła się dopiero od chunka
    /// wyzwalającego. Statystyka obejmuje za to czas SPRZED okna śledzenia
    /// dołków (a przy cięciu wymuszonym również chunki ogona zarejestrowane
    /// już po punkcie cięcia). Dlatego niskie `p_min` NIE dowodzi, że w oknie
    /// śledzenia był dołek nadający się do cięcia — głębokie minima potrafią
    /// leżeć w całości przed oknem.
    pub p_min: f32,
    /// średnie p w segmencie (ten sam zakres liczenia co `p_min`)
    pub p_mean: f32,
    /// ile chunków objęła statystyka `p_min`/`p_mean`; `p_n * 32 ms` mówi
    /// wprost, ile audio ta statystyka naprawdę opisuje. Bez tego p̄ sugeruje
    /// pokrycie całego bufora, a rozjazd jest systematyczny z DWÓCH powodów:
    /// (1) zawsze krótsze od bufora o faktycznie wlany preroll (do
    /// `preroll_ms`, przy domyślnych 300 ms to 9 chunków = 288 ms), bo
    /// preroll nie przechodzi przez `note_p`; (2) po cięciu wymuszonym
    /// liczniki startują od zera, choć bufor ogona zawiera już nakładkę
    /// i ogon sprzed cięcia.
    ///
    /// POLE WYŁĄCZNIE DO LOGOWANIA — nie wolno go czytać w żadnym predykacie
    /// decydującym o cięciu (o tym rozstrzygają wyłącznie `silence_ms`,
    /// `settled_dip`, `hard_max_ms` i `min_speech_ms || tail_of_forced_cut`).
    pub p_n: u32,
    /// p DOKŁADNIE w punkcie cięcia (dno dołka, w którym przecięliśmy bufor).
    /// `None` dla domknięć nie-wymuszonych: pauza i mikropauza kończą segment
    /// na jego końcu, a nie w wybranym dołku — nie ma tam "punktu cięcia"
    /// w tym sensie. `None` bywa też przy cięciu WYMUSZONYM, jeśli okno
    /// śledzenia dołków jeszcze się nie zaczęło (`dip == None`) — to możliwe
    /// tylko przy konfiguracji łamiącej niezmiennik z `config.rs`
    /// (`hard_max_ms > soft_max_ms - dip_settle_ms`), którego nic nie
    /// waliduje; wtedy hard-max tnie na końcu bufora, a ogon jest pusty.
    /// To jedyna liczba rozstrzygająca, czy inny `dip_threshold` cokolwiek
    /// by zmienił przy cięciach hard-max — ale rozstrzyga się to ANALIZĄ
    /// LOGÓW, nie w kodzie.
    ///
    /// POLE WYŁĄCZNIE DO LOGOWANIA — nie wolno go czytać w żadnym predykacie
    /// decydującym o cięciu (lista predykatów jak przy `p_n` wyżej). Wpięcie
    /// go w decyzję zmienia zachowanie toru i unieważnia porównania z już
    /// zebranymi logami.
    pub p_dip: Option<f32>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    /// naturalna cisza (pełny hangover_ms)
    Hangover,
    /// mikropauza po przekroczeniu soft_max_ms (soft_hangover_ms)
    SoftHangover,
    /// cięcie w ustabilizowanym dołku p: minimum okna zeszło poniżej
    /// dip_threshold i utrzymało się przez dip_settle_ms
    SettledDip,
    /// twarde cięcie przy hard_max_ms. NIE znaczy "żaden dołek się nie
    /// trafił" i NIE tnie w dowolnym miejscu: punkt cięcia jest ten sam co
    /// przy SettledDip — biegnące minimum p okna śledzenia (wspólne `cut`
    /// w `maybe_force_cut`). Różni się wyłącznie powód odpalenia: to minimum
    /// nie spełniło dip_threshold albo nie zdążyło przetrwać dip_settle_ms,
    /// więc o chwili cięcia zdecydował limit długości, a nie akustyka —
    /// dołek bywa wtedy płytki i wypada wewnątrz frazy.
    ///
    /// ZASTRZEŻENIE: powyższe obowiązuje, dopóki trzymany jest niezmiennik
    /// z `config.rs` (`hard_max_ms > soft_max_ms - dip_settle_ms`), którego
    /// NIC nie waliduje. Przy jego naruszeniu hard_max odpala, zanim okno
    /// śledzenia w ogóle się zacznie — wtedy cięcie idzie na końcu bufora,
    /// z pustym ogonem i `p_dip == None`.
    HardMax,
}

impl CloseReason {
    pub fn label(self) -> &'static str {
        match self {
            CloseReason::Hangover => "pauza",
            CloseReason::SoftHangover => "mikropauza",
            CloseReason::SettledDip => "dołek",
            CloseReason::HardMax => "hard-max",
        }
    }
}

enum State {
    Idle,
    Speech,
    Hangover { silence_ms: u32 },
}

pub struct Segmenter {
    cfg: VadCfg,
    state: State,
    preroll: VecDeque<Vec<f32>>,
    preroll_chunks: usize,
    seg: Vec<f32>,
    /// łączna długość bufora `seg` (preroll + mowa + cisze) — steruje progami cięcia
    seg_ms: u32,
    /// ms rozpoznanej MOWY w bieżącym segmencie (bez prerollu i ciszy hangoveru) —
    /// to na tym progu opiera się min_speech_ms, żeby preroll/cisza go nie zawyżały
    speech_ms: u32,
    /// najniższe p od miękkiego limitu: (pozycja w próbkach, p, seg_ms w
    /// chwili wystąpienia) — ten trzeci element pozwala ciąć w "ustabilizowanym"
    /// dołku bez czekania na hard_max
    dip: Option<(usize, f32, u32)>,
    /// statystyki p bieżącego segmentu (diagnostyka; patrz Utterance)
    p_min_seg: f32,
    p_sum: f64,
    p_n: u32,
    /// bieżący segment to ogon po cięciu wymuszonym — zawiera realną mowę
    /// (nakładka + audio od dołka), więc przy domknięciu NIE wolno go
    /// odrzucić progiem min_speech_ms: licznik mowy ogona startuje od zera
    /// i krótka końcówka frazy ("...see you tomorrow" → "tomorrow")
    /// przepadałaby bez transkrypcji
    tail_of_forced_cut: bool,
    /// generacja bieżącego (otwartego) segmentu — rośnie przy KAŻDYM
    /// domknięciu (pauza, odrzucenie za-krótkiego, cięcie wymuszone);
    /// spekulacyjne STT odrzuca w całości wyniki przebiegów ze starszą
    /// generacją, bo dotyczą audio, którego już nie ma w buforze
    gen: u64,
}

impl Segmenter {
    pub fn new(cfg: VadCfg) -> Self {
        let preroll_chunks = (cfg.preroll_ms / CHUNK_MS).max(1) as usize;
        Self {
            cfg,
            state: State::Idle,
            preroll: VecDeque::with_capacity(preroll_chunks + 1),
            preroll_chunks,
            seg: Vec::new(),
            seg_ms: 0,
            speech_ms: 0,
            dip: None,
            p_min_seg: f32::INFINITY,
            p_sum: 0.0,
            p_n: 0,
            tail_of_forced_cut: false,
            gen: 1, // pierwszy segment = generacja 1 (logi fragmentów "#1.0")
        }
    }

    fn note_p(&mut self, p: f32) {
        self.p_min_seg = self.p_min_seg.min(p);
        self.p_sum += p as f64;
        self.p_n += 1;
    }

    /// (p_min, p_mean, p_n) bieżącego segmentu + reset liczników pod następny.
    /// `p_n` wychodzi na zewnątrz, bo statystyka nigdy nie pokrywa całego
    /// bufora: preroll wlewa się do `seg` z pominięciem `note_p`, a po cięciu
    /// wymuszonym liczniki startują od zera, choć bufor ogona już zawiera
    /// audio (nakładka + ogon) — bez tej liczby p̄ wygląda, jakby opisywało
    /// cały bufor.
    fn take_p_stats(&mut self) -> (f32, f32, u32) {
        let mean = if self.p_n > 0 {
            (self.p_sum / self.p_n as f64) as f32
        } else {
            0.0
        };
        let min = if self.p_min_seg.is_finite() { self.p_min_seg } else { 0.0 };
        let n = self.p_n;
        self.p_min_seg = f32::INFINITY;
        self.p_sum = 0.0;
        self.p_n = 0;
        (min, mean, n)
    }

    /// Czy segmenter nie ma otwartej wypowiedzi (bezpiecznie można przestać
    /// dostarczać audio, np. gdy aplikacja źródłowa jest w pauzie).
    pub fn is_idle(&self) -> bool {
        matches!(self.state, State::Idle)
    }

    /// Migawka OTWARTEGO segmentu dla spekulacyjnego STT: (generacja, kopia
    /// całego bufora audio, ms rozpoznanej mowy, czy to ogon po cięciu
    /// wymuszonym). None gdy nic nie jest otwarte. Kopia, nie pożyczka —
    /// wątek STT pracuje na niej, gdy bufor dalej rośnie. Bramkę min_open_ms
    /// stosuje WYWOŁUJĄCY (tam mieszka config STT); koszt klonu (maks.
    /// ~512 KB co kadencję) jest pomijalny.
    ///
    /// Czwarty element (`tail_of_forced_cut`) MUSI trafić do bramki
    /// spekulacji tak samo jak trafia do bramki domknięcia niżej
    /// (`speech_ms >= min_speech_ms || tail_of_forced_cut`) — bez niego
    /// bufor bezpośrednio po cięciu wymuszonym (nakładka + ogon, realna
    /// potwierdzona mowa) czeka na spekulację tak, jakby był świeżą,
    /// niepotwierdzoną ciszą, bo `speech_ms` startuje wtedy od zera.
    pub fn open_snapshot(&self) -> Option<(u64, Vec<f32>, u32, bool)> {
        if matches!(self.state, State::Idle) {
            return None;
        }
        Some((self.gen, self.seg.clone(), self.speech_ms, self.tail_of_forced_cut))
    }

    /// Podaj kolejne okno 512 próbek wraz z prawdopodobieństwem mowy z VAD.
    /// Zwraca segment, gdy wypowiedź została domknięta.
    pub fn push_chunk(&mut self, chunk: &[f32], p: f32) -> Option<Utterance> {
        debug_assert_eq!(chunk.len(), VAD_CHUNK);
        match self.state {
            State::Idle => {
                if p >= self.cfg.threshold_enter {
                    self.seg.clear();
                    self.seg_ms = 0;
                    // chunk wyzwalający ma p >= threshold_enter > threshold_exit — na pewno mowa
                    self.speech_ms = CHUNK_MS;
                    for c in self.preroll.drain(..) {
                        self.seg.extend_from_slice(&c);
                        self.seg_ms += CHUNK_MS;
                    }
                    self.push_speech(chunk);
                    self.note_p(p);
                    self.state = State::Speech;
                } else {
                    self.preroll.push_back(chunk.to_vec());
                    if self.preroll.len() > self.preroll_chunks {
                        self.preroll.pop_front();
                    }
                }
                None
            }
            State::Speech => {
                self.push_speech(chunk);
                self.note_p(p);
                self.track_dip(p);
                if p >= self.cfg.threshold_exit {
                    self.speech_ms += CHUNK_MS;
                } else {
                    self.state = State::Hangover {
                        silence_ms: CHUNK_MS,
                    };
                }
                self.maybe_force_cut(None)
            }
            State::Hangover { silence_ms } => {
                self.push_speech(chunk);
                self.note_p(p);
                self.track_dip(p);
                // Histereza: powrót do mowy już przy progu PODTRZYMANIA (exit), nie
                // progu wejścia (enter) — inaczej p w [exit, enter) podczas hangoveru
                // (typowe dla mowy pod podkładem muzycznym) liczyłby się jako cisza
                // i tnie wypowiedzi w pół zdania.
                if p >= self.cfg.threshold_exit {
                    self.speech_ms += CHUNK_MS;
                    self.state = State::Speech;
                    return self.maybe_force_cut(None);
                }
                let silence_ms = silence_ms + CHUNK_MS;
                // Pełny hangover dla zwykłej mowy; skrócony, gdy segment już
                // przekroczył soft_max — szybcy mówcy robią między frazami
                // pauzy rzędu 200-400 ms, za krótkie na pełny hangover, przez
                // co ciągła narracja czekała aż do twardego cięcia przy
                // hard_max (czyli ~8 s opóźnienia zanim COKOLWIEK wyszło
                // z segmentera). Cięcie na realnej pauzie daje przy okazji
                // lepszą granicę frazy niż dołek prawdopodobieństwa.
                let hangover_needed = if self.seg_ms >= self.cfg.soft_max_ms {
                    self.cfg.soft_hangover_ms.min(self.cfg.hangover_ms)
                } else {
                    self.cfg.hangover_ms
                };
                if silence_ms >= hangover_needed {
                    let (p_min, p_mean, p_n) = self.take_p_stats();
                    // M1: KAŻDE domknięcie inkrementuje generację — także
                    // odrzucenie za-krótkiego segmentu niżej: migawki pobrane
                    // z odrzuconego bufora muszą stać się nieważne, inaczej
                    // przebieg częściowy odrzuconego audio skleiłby się
                    // w trackerze z następnym segmentem
                    let closing_gen = self.gen;
                    self.gen += 1;
                    let out = if self.speech_ms >= self.cfg.min_speech_ms
                        || self.tail_of_forced_cut
                    {
                        Some(Utterance {
                            audio: std::mem::take(&mut self.seg),
                            gen: closing_gen,
                            forced: false,
                            reason: if hangover_needed < self.cfg.hangover_ms {
                                CloseReason::SoftHangover
                            } else {
                                CloseReason::Hangover
                            },
                            p_min,
                            p_mean,
                            p_n,
                            // pauza/mikropauza tną na końcu bufora, nie
                            // w wybranym dołku — punkt cięcia nie istnieje
                            p_dip: None,
                        })
                    } else {
                        // za krótkie — niemal na pewno śmieć/halucynacja
                        self.seg.clear();
                        None
                    };
                    self.seg_ms = 0;
                    self.speech_ms = 0;
                    self.dip = None;
                    self.tail_of_forced_cut = false;
                    self.state = State::Idle;
                    out
                } else {
                    self.state = State::Hangover { silence_ms };
                    self.maybe_force_cut(Some(silence_ms))
                }
            }
        }
    }

    fn push_speech(&mut self, chunk: &[f32]) {
        self.seg.extend_from_slice(chunk);
        self.seg_ms += CHUNK_MS;
    }

    fn track_dip(&mut self, p: f32) {
        // Dołki śledzone już od soft_max − dip_settle: dołek tuż sprzed
        // progu zdąży się "ustabilizować" dokładnie w chwili osiągnięcia
        // soft_max i cięcie odpala od razu, zamiast czekać na następny
        // (który może przyjść sekundy później — stąd 5-sekundowe segmenty
        // i dziury w tłumaczeniu przy ciągłej mowie). Cięcie i tak nie
        // nastąpi przed soft_max: settled_dip wymaga dip_settle_ms od
        // wystąpienia dołka.
        if self.seg_ms + self.cfg.dip_settle_ms >= self.cfg.soft_max_ms
            && self.dip.map_or(true, |(_, best, _)| p < best)
        {
            self.dip = Some((self.seg.len(), p, self.seg_ms));
        }
    }

    /// Twarde cięcie długiej mowy: tniemy w dołku prawdopodobieństwa
    /// (granica frazy) i przenosimy nakładkę do następnego segmentu.
    ///
    /// `hangover_silence`: jeśli cięcie następuje w trakcie hangoveru, ile ms
    /// ciszy już naliczono — po cięciu stan MUSI wrócić do tego samego
    /// Hangoveru (z zachowanym licznikiem), a nie bezwarunkowo do Speech,
    /// inaczej cisza po cięciu zostaje błędnie zinterpretowana jako nowa mowa
    /// i po whisperze ląduje śmieciowy segment złożony głównie z ciszy.
    fn maybe_force_cut(&mut self, hangover_silence: Option<u32>) -> Option<Utterance> {
        // Cięcie w "ustabilizowanym" dołku: jeśli po soft_max trafił się
        // prawdziwy akustyczny dołek (p <= dip_threshold — oddech/zawieszenie
        // za krótkie, żeby domknąć segment przez soft_hangover) i od tej
        // chwili minęło dip_settle_ms bez głębszego dołka — tnij w nim TERAZ.
        // Bez tego ciągła mowa bez pauz czekała zawsze pełne hard_max (~8 s),
        // zanim COKOLWIEK wyszło z segmentera, mimo że miejsce cięcia było
        // znane dużo wcześniej.
        //
        // UWAGA na progi: warunek dołka porównuje się z `dip_threshold`
        // (luźniejszym), a NIE z `threshold_exit` — ten drugi rządzi tylko
        // histerezą Speech/Hangover.
        //
        // Czym różni się hard_max od dołka: NIE tym, że "żaden dołek się nie
        // trafił". `track_dip` zasiewa `dip` bezwarunkowo pierwszym chunkiem
        // od `soft_max - dip_settle`, więc przy `seg_ms >= hard_max` dołek
        // praktycznie zawsze istnieje — po prostu nie zszedł poniżej
        // dip_threshold albo nie zdążył przeżyć dip_settle_ms. Obie ścieżki
        // tną wtedy w TYM SAMYM punkcie (wspólne `cut` niżej = biegnące
        // minimum p okna); hard_max zmienia wyłącznie to, że przestajemy
        // czekać na spełnienie warunków dołka.
        //
        // Ale to zdanie jest prawdziwe WARUNKOWO — tylko przy zachowanym
        // niezmienniku z config.rs `hard_max_ms > soft_max_ms - dip_settle_ms`,
        // którego NIC w kodzie nie sprawdza (jedyna walidacja strojenia to
        // `Config::tuning_warning`, o czym innym). Konfiguracja np.
        // soft_max=6000 / dip_settle=500 / hard_max=4000 przechodzi przez
        // parser bez słowa protestu, a wtedy przy seg_ms >= hard_max okno
        // śledzenia jeszcze się nie zaczęło, `dip` jest None i gałąź
        // `unwrap_or(self.seg.len())` NIE jest martwa: hard-max tnie na końcu
        // bufora, ogon wychodzi pusty, a `p_dip` None mimo `forced: true`.
        // Przy domyślnym strojeniu ta gałąź się nie wykonuje i zostaje jako
        // asekuracja właśnie na taki rozjazd konfiguracji.
        let settled_dip = self.dip.is_some_and(|(_, p, at_ms)| {
            p <= self.cfg.dip_threshold
                && self.seg_ms.saturating_sub(at_ms) >= self.cfg.dip_settle_ms
        });
        if self.seg_ms < self.cfg.hard_max_ms && !settled_dip {
            return None;
        }
        // p w punkcie cięcia przechwytujemy RAZEM z pozycją: niżej stoi
        // `self.dip = None`, więc każdy odczyt po tamtej linii dawałby
        // zawsze None (dokładnie ten artefakt, przez który logowane pmin
        // było bezużyteczne dla diagnozy hard-maxa)
        let (cut, p_dip) = self
            .dip
            .map(|(i, p, _)| (i, Some(p)))
            .unwrap_or((self.seg.len(), None));
        let cut = cut.min(self.seg.len());
        let head: Vec<f32> = self.seg[..cut].to_vec();
        let tail: Vec<f32> = self.seg[cut..].to_vec();

        let overlap_samples = (self.cfg.overlap_ms as usize * VAD_RATE / 1000).min(head.len());
        self.seg.clear();
        self.seg
            .extend_from_slice(&head[head.len() - overlap_samples..]);
        self.seg.extend_from_slice(&tail);
        self.seg_ms = (self.seg.len() * 1000 / VAD_RATE) as u32;
        self.dip = None;
        // liczymy mowę nowego segmentu od zera od tego miejsca — nakładka i ogon
        // dostaną realne przypisanie do speech_ms w kolejnych wywołaniach push_chunk
        self.speech_ms = 0;
        self.state = match hangover_silence {
            Some(silence_ms) => State::Hangover { silence_ms },
            None => State::Speech,
        };

        // statystyki dotyczą chunków zarejestrowanych przez `note_p` od
        // poprzedniego domknięcia (głowa BEZ prerollu + zarejestrowany dotąd
        // ogon) — dla diagnostyki wystarczające; ogon zaczyna liczyć od zera,
        // więc jego przyszłe p̄/pmin opiszą tylko audio zarejestrowane PO
        // cięciu, mimo że bufor ma już nakładkę i ogon — stąd `p_n`
        // w Utterance, żeby było widać pokrycie
        let (p_min, p_mean, p_n) = self.take_p_stats();
        self.tail_of_forced_cut = true;
        // M1: cięcie wymuszone łamie założenie append-only bufora — nowy
        // bufor zaczyna się nakładką akustycznie powielającą słowa być może
        // już scommitowane; granica generacji wymusza w trackerze twardy
        // reset, ŻADNEGO re-kotwiczenia przez tę granicę (duplikat
        // z nakładki jest akceptowany projektowo)
        let closing_gen = self.gen;
        self.gen += 1;
        Some(Utterance {
            audio: head,
            gen: closing_gen,
            forced: true,
            reason: if settled_dip {
                CloseReason::SettledDip
            } else {
                CloseReason::HardMax
            },
            p_min,
            p_mean,
            p_n,
            p_dip,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// progi celowo malutkie (wielokrotności chunka 32 ms), żeby testy
    /// obywały się bez Silero — push_chunk przyjmuje p jawnie
    fn cfg() -> VadCfg {
        VadCfg {
            threshold_enter: 0.5,
            threshold_exit: 0.35,
            preroll_ms: 32,
            min_speech_ms: 32,
            hangover_ms: 64,
            soft_hangover_ms: 64,
            dip_settle_ms: 32,
            dip_threshold: 0.6,
            soft_max_ms: 320,
            hard_max_ms: 320,
            overlap_ms: 32,
        }
    }

    fn chunk(v: f32) -> Vec<f32> {
        vec![v; VAD_CHUNK]
    }

    /// mowa + cisza aż do domknięcia hangoverem; zwraca domknięty segment
    fn speak_and_close(s: &mut Segmenter, speech_chunks: usize) -> Option<Utterance> {
        let c = chunk(0.0);
        let mut out = None;
        for _ in 0..speech_chunks {
            assert!(s.push_chunk(&c, 0.9).is_none());
        }
        for _ in 0..2 {
            if let Some(u) = s.push_chunk(&c, 0.0) {
                out = Some(u);
            }
        }
        out
    }

    // V1: domknięcie hangoverem → gen 1; kolejny segment → gen 2
    #[test]
    fn v1_hangover_inkrementuje_generacje() {
        let mut s = Segmenter::new(cfg());
        let u1 = speak_and_close(&mut s, 4).expect("segment 1 domknięty");
        assert!(!u1.forced);
        assert_eq!(u1.gen, 1);
        let u2 = speak_and_close(&mut s, 4).expect("segment 2 domknięty");
        assert_eq!(u2.gen, 2);
    }

    // V2: cięcie wymuszone inkrementuje gen; migawka po cięciu należy już
    // do NOWEJ generacji, a jej audio zaczyna się nakładką z głowy
    #[test]
    fn v2_forced_cut_nowa_generacja_i_naklada() {
        let mut s = Segmenter::new(cfg());
        let mut forced = None;
        // ciągła mowa bez dołków — hard_max (320 ms = 10 chunków) tnie na
        // pozycji ostatniego śledzonego dołka (koniec chunka nr 8)
        for i in 0..10 {
            let c = chunk(i as f32 * 0.001);
            if let Some(u) = s.push_chunk(&c, 0.9) {
                forced = Some((i, u));
            }
        }
        let (at, u) = forced.expect("hard_max powinien wymusić cięcie");
        assert_eq!(at, 9);
        assert!(u.forced);
        assert_eq!(u.gen, 1);
        let (gen, audio, _, tail_of_forced_cut) = s.open_snapshot().expect("mowa trwa dalej");
        assert_eq!(gen, 2);
        assert!(tail_of_forced_cut);
        // nowy bufor = nakładka 32 ms (ostatnie 512 próbek głowy, chunk nr 8)
        // + ogon (chunk nr 9)
        assert_eq!(audio.len(), 2 * VAD_CHUNK);
        assert_eq!(audio[0], 8.0 * 0.001);
        assert_eq!(audio[VAD_CHUNK], 9.0 * 0.001);
    }

    // V2b: cięcie hard-max niesie p DOŁKA, w którym faktycznie przecięliśmy
    // bufor — nie None i nie globalne minimum segmentu. Głęboki dołek (0.05)
    // leży PRZED oknem śledzenia, więc p_min go widzi, a punkt cięcia nie:
    // dokładnie ta różnica sprawiła, że logowane pmin nie mówiło nic
    // o przyczynie cięć hard-max.
    #[test]
    fn v2b_hard_max_niesie_p_punktu_ciecia() {
        let mut s = Segmenter::new(cfg());
        // p po chunkach: dołek 0.05 poza oknem (i=4), dołek okna 0.75 (i=8)
        let ps = [0.9, 0.9, 0.9, 0.9, 0.05, 0.9, 0.9, 0.9, 0.75, 0.8];
        let mut forced = None;
        for (i, p) in ps.iter().enumerate() {
            let c = chunk(i as f32 * 0.001);
            if let Some(u) = s.push_chunk(&c, *p) {
                forced = Some(u);
            }
        }
        let u = forced.expect("hard_max powinien wymusić cięcie");
        assert!(u.forced);
        assert_eq!(u.reason.label(), "hard-max");
        // p w punkcie cięcia = minimum OKNA (0.75), nie minimum segmentu
        assert_eq!(u.p_dip, Some(0.75));
        assert_eq!(u.p_min, 0.05);
        // statystyka objęła wszystkie 10 chunków (głowa 9 + ogon 1)
        assert_eq!(u.p_n, 10);
        // cięcie faktycznie w dołku okna: głowa = 9 chunków
        assert_eq!(u.audio.len(), 9 * VAD_CHUNK);
    }

    // V2c: cięcie w ustabilizowanym dołku też niesie p punktu cięcia
    #[test]
    fn v2c_settled_dip_niesie_p_dolka() {
        let mut vcfg = cfg();
        vcfg.hard_max_ms = 3_200; // hard-max daleko — cięcie ma odpalić dołek
        let mut s = Segmenter::new(vcfg);
        let ps = [0.9, 0.9, 0.9, 0.9, 0.2, 0.9, 0.9, 0.9, 0.5, 0.9];
        let mut forced = None;
        for (i, p) in ps.iter().enumerate() {
            let c = chunk(i as f32 * 0.001);
            if let Some(u) = s.push_chunk(&c, *p) {
                forced = Some(u);
            }
        }
        let u = forced.expect("ustabilizowany dołek powinien wymusić cięcie");
        assert_eq!(u.reason.label(), "dołek");
        assert_eq!(u.p_dip, Some(0.5));
        assert_eq!(u.p_min, 0.2);
    }

    // V2d: domknięcie pauzą nie ma punktu cięcia — p_dip jest None
    #[test]
    fn v2d_hangover_bez_punktu_ciecia() {
        let mut s = Segmenter::new(cfg());
        let u = speak_and_close(&mut s, 4).expect("segment domknięty");
        assert!(!u.forced);
        assert!(u.p_dip.is_none());
        // 4 chunki mowy + 2 ciszy; preroll pusty (pierwszy segment sesji)
        assert_eq!(u.p_n, 6);
        assert_eq!(u.audio.len(), 6 * VAD_CHUNK);
    }

    // V2e: preroll jest w BUFORZE, ale nie w statystyce p — `p_n * 32 ms`
    // jest systematycznie krótsze od segmentu o wlany preroll. Bez tego
    // testu komentarz przy `p_n` mówiłby co innego niż kod, a czytelnik
    // logu wziąłby stały rozjazd ~preroll_ms za nowy błąd.
    #[test]
    fn v2e_preroll_nie_wchodzi_do_statystyki_p() {
        let mut s = Segmenter::new(cfg()); // preroll_ms = 32 → 1 chunk
        let c = chunk(0.0);
        // cisza przed mową trafia do kolejki prerollu (bez note_p)
        assert!(s.push_chunk(&c, 0.0).is_none());
        let u = speak_and_close(&mut s, 4).expect("segment domknięty");
        // bufor: 1 preroll + 4 mowy + 2 ciszy = 7 chunków
        assert_eq!(u.audio.len(), 7 * VAD_CHUNK);
        // statystyka: tylko 6 — preroll pominięty
        assert_eq!(u.p_n, 6);
    }

    // V3: open_snapshot — None w Idle, rosnąca kopia w Speech
    #[test]
    fn v3_snapshot_idle_none_potem_rosnie() {
        let mut s = Segmenter::new(cfg());
        assert!(s.open_snapshot().is_none());
        let c = chunk(0.0);
        assert!(s.push_chunk(&c, 0.9).is_none());
        let (gen, a1, speech_ms, tail_of_forced_cut) =
            s.open_snapshot().expect("segment otwarty");
        assert_eq!(gen, 1);
        assert_eq!(a1.len(), VAD_CHUNK);
        assert_eq!(speech_ms, 32);
        assert!(!tail_of_forced_cut);
        assert!(s.push_chunk(&c, 0.9).is_none());
        let (_, a2, _, _) = s.open_snapshot().expect("segment nadal otwarty");
        assert_eq!(a2.len(), 2 * VAD_CHUNK);
    }

    // V3b: odrzucenie za-krótkiego segmentu TEŻ inkrementuje gen — migawki
    // pobrane z odrzuconego bufora muszą stać się nieważne
    #[test]
    fn v3b_odrzucenie_krotkiego_inkrementuje() {
        let mut vcfg = cfg();
        vcfg.min_speech_ms = 96; // wymagane 3 chunki mowy
        let mut s = Segmenter::new(vcfg);
        let c = chunk(0.0);
        // 1 chunk mowy (32 ms < 96) + cisza do hangoveru → odrzucenie bez Utterance
        assert!(s.push_chunk(&c, 0.9).is_none());
        assert!(s.push_chunk(&c, 0.0).is_none());
        assert!(s.push_chunk(&c, 0.0).is_none());
        assert!(s.is_idle());
        // następny segment dostaje generację 2 — odrzucenie skonsumowało 1
        assert!(s.push_chunk(&c, 0.9).is_none());
        let (gen, _, _, _) = s.open_snapshot().expect("segment otwarty");
        assert_eq!(gen, 2);
    }
}
