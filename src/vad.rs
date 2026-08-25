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
    /// segment ucięty twardym limitem (mowa trwa dalej)
    pub forced: bool,
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
    /// (pozycja w próbkach, prawdopodobieństwo) najniższego p od miękkiego limitu
    dip: Option<(usize, f32)>,
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
        }
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
                    let out = if self.speech_ms >= self.cfg.min_speech_ms {
                        Some(Utterance {
                            audio: std::mem::take(&mut self.seg),
                            forced: false,
                        })
                    } else {
                        // za krótkie — niemal na pewno śmieć/halucynacja
                        self.seg.clear();
                        None
                    };
                    self.seg_ms = 0;
                    self.speech_ms = 0;
                    self.dip = None;
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
        if self.seg_ms >= self.cfg.soft_max_ms
            && self.dip.map_or(true, |(_, best)| p < best)
        {
            self.dip = Some((self.seg.len(), p));
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
        if self.seg_ms < self.cfg.hard_max_ms {
            return None;
        }
        let cut = self.dip.map(|(i, _)| i).unwrap_or(self.seg.len());
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

        Some(Utterance {
            audio: head,
            forced: true,
        })
    }
}
