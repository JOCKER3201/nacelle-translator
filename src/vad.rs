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
    /// segment ucięty w trakcie mowy (mowa trwa dalej)
    pub forced: bool,
    /// jak doszło do domknięcia — kluczowa dana diagnostyczna dla trudnego
    /// materiału (film z muzyką): dominacja hard-max przy wysokim p_min
    /// oznacza, że podkład trzyma p nad progami i VAD nie ma się o co zaczepić
    pub reason: CloseReason,
    /// najniższe p zaobserwowane w segmencie
    pub p_min: f32,
    /// średnie p w segmencie
    pub p_mean: f32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    /// naturalna cisza (pełny hangover_ms)
    Hangover,
    /// mikropauza po przekroczeniu soft_max_ms (soft_hangover_ms)
    SoftHangover,
    /// cięcie w ustabilizowanym dołku p (dip_settle_ms + dip_threshold)
    SettledDip,
    /// awaryjne twarde cięcie przy hard_max_ms — żaden dołek się nie trafił
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
        }
    }

    fn note_p(&mut self, p: f32) {
        self.p_min_seg = self.p_min_seg.min(p);
        self.p_sum += p as f64;
        self.p_n += 1;
    }

    /// (p_min, p_mean) bieżącego segmentu + reset liczników pod następny.
    fn take_p_stats(&mut self) -> (f32, f32) {
        let mean = if self.p_n > 0 {
            (self.p_sum / self.p_n as f64) as f32
        } else {
            0.0
        };
        let min = if self.p_min_seg.is_finite() { self.p_min_seg } else { 0.0 };
        self.p_min_seg = f32::INFINITY;
        self.p_sum = 0.0;
        self.p_n = 0;
        (min, mean)
    }

    /// Czy segmenter nie ma otwartej wypowiedzi (bezpiecznie można przestać
    /// dostarczać audio, np. gdy aplikacja źródłowa jest w pauzie).
    pub fn is_idle(&self) -> bool {
        matches!(self.state, State::Idle)
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
                    let (p_min, p_mean) = self.take_p_stats();
                    let out = if self.speech_ms >= self.cfg.min_speech_ms
                        || self.tail_of_forced_cut
                    {
                        Some(Utterance {
                            audio: std::mem::take(&mut self.seg),
                            forced: false,
                            reason: if hangover_needed < self.cfg.hangover_ms {
                                CloseReason::SoftHangover
                            } else {
                                CloseReason::Hangover
                            },
                            p_min,
                            p_mean,
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
        // prawdziwy akustyczny dołek (p <= threshold_exit — oddech/zawieszenie
        // za krótkie, żeby domknąć segment przez soft_hangover) i od tej
        // chwili minęło dip_settle_ms bez głębszego dołka — tnij w nim TERAZ.
        // Bez tego ciągła mowa bez pauz czekała zawsze pełne hard_max (~8 s),
        // zanim COKOLWIEK wyszło z segmentera, mimo że miejsce cięcia było
        // znane dużo wcześniej. hard_max zostaje jako bezpiecznik na wypadek,
        // gdy żaden dołek nie zejdzie poniżej threshold_exit.
        let settled_dip = self.dip.is_some_and(|(_, p, at_ms)| {
            p <= self.cfg.dip_threshold
                && self.seg_ms.saturating_sub(at_ms) >= self.cfg.dip_settle_ms
        });
        if self.seg_ms < self.cfg.hard_max_ms && !settled_dip {
            return None;
        }
        let cut = self.dip.map(|(i, _, _)| i).unwrap_or(self.seg.len());
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

        // statystyki dotyczą całego dotychczasowego bufora (głowa+ogon) —
        // dla diagnostyki wystarczające; ogon zaczyna liczyć od zera
        let (p_min, p_mean) = self.take_p_stats();
        self.tail_of_forced_cut = true;
        Some(Utterance {
            audio: head,
            forced: true,
            reason: if settled_dip {
                CloseReason::SettledDip
            } else {
                CloseReason::HardMax
            },
            p_min,
            p_mean,
        })
    }
}
