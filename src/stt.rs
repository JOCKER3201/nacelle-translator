//! Transkrypcja: whisper-rs 0.16 (whisper.cpp) + filtr antyhalucynacyjny.

use anyhow::{Context, Result};
use std::path::Path;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperState};

/// whisper.cpp zwraca 0 segmentów dla audio krótszego niż ~1 s —
/// segmenty VAD dopadowujemy zerami do tej długości.
pub const MIN_SAMPLES: usize = (1.1 * 16_000.0) as usize;

pub struct Transcript {
    pub text: String,
    /// kod wykrytego języka ("en", "pl", ...) albo "und"
    pub lang: String,
    pub no_speech_prob: f32,
    pub avg_logprob: f32,
}

pub struct Transcriber {
    // WhisperState trzyma cache KV dekodera — tworzenie go per segment
    // (co kilka sekund) to zbędna alokacja/zwolnienie na krytycznej ścieżce
    // latencji. `set_no_context(true)` już zapewnia izolację między
    // kolejnymi wywołaniami `full()`, więc reużycie jest bezpieczne.
    state: WhisperState,
    threads: i32,
    language: String,
}

impl Transcriber {
    pub fn new(model: &Path, threads: i32, language: &str) -> Result<Self> {
        let mut cp = WhisperContextParameters::default();
        cp.use_gpu(true); // backend CUDA (feature "cuda" na whisper-rs w Cargo.toml)
        let ctx = WhisperContext::new_with_params(model, cp)
            .with_context(|| format!("nie mogę załadować modelu whisper: {}", model.display()))?;
        let state = ctx.create_state().context("whisper create_state")?;
        Ok(Self {
            state,
            threads,
            language: language.to_string(),
        })
    }

    /// `samples`: f32 mono 16 kHz w [-1, 1].
    pub fn transcribe(&mut self, samples: &[f32]) -> Result<Transcript> {
        let padded;
        let samples = if samples.len() < MIN_SAMPLES {
            padded = {
                let mut v = samples.to_vec();
                v.resize(MIN_SAMPLES, 0.0);
                v
            };
            &padded[..]
        } else {
            samples
        };

        let mut p = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        p.set_n_threads(self.threads);
        if self.language == "auto" {
            p.set_language(Some("auto")); // autodetekcja + transkrypcja
        } else {
            p.set_language(Some(&self.language));
        }
        p.set_detect_language(false); // true = sama detekcja, bez tekstu
        p.set_translate(false); // translate whisperem umie tylko na angielski
        p.set_no_context(true); // izolacja między wywołaniami full()
        p.set_single_segment(true);
        p.set_suppress_blank(true);
        p.set_suppress_nst(true); // tokeny typu [Music]/(applause)
        p.set_token_timestamps(false);
        p.set_temperature(0.0);
        p.set_temperature_inc(0.2);
        p.set_no_speech_thold(0.6);
        p.set_logprob_thold(-1.0);
        p.set_print_special(false);
        p.set_print_progress(false);
        p.set_print_realtime(false);
        p.set_print_timestamps(false);

        self.state.full(p, samples).context("whisper full()")?;

        let lang_id = self.state.full_lang_id_from_state();
        let lang = whisper_rs::get_lang_str(lang_id)
            .unwrap_or("und")
            .to_string();

        let mut text = String::new();
        let mut no_speech = 0.0f32;
        let (mut plog_sum, mut n_tok) = (0.0f64, 0u32);
        for seg in self.state.as_iter() {
            no_speech = no_speech.max(seg.no_speech_probability());
            if let Ok(s) = seg.to_str() {
                text.push_str(s);
            }
            for i in 0..seg.n_tokens() {
                if let Some(tok) = seg.get_token(i) {
                    plog_sum += tok.token_data().plog as f64;
                    n_tok += 1;
                }
            }
        }
        let avg_logprob = if n_tok > 0 {
            (plog_sum / n_tok as f64) as f32
        } else {
            -10.0
        };

        Ok(Transcript {
            text: text.trim().to_string(),
            lang,
            no_speech_prob: no_speech,
            avg_logprob,
        })
    }
}

/// Znane frazy-halucynacje whispera przy ciszy/muzyce.
const BLACKLIST: &[&str] = &[
    "thanks for watching",
    "thank you for watching",
    "please subscribe",
    "subtitles by the amara.org community",
    "napisy stworzone przez społeczność amara.org",
    "dziękuję za obejrzenie",
    "dziękuję za oglądanie",
    "dziękujemy za oglądanie",
    "zapraszam na kolejny odcinek",
    "zapraszam na kolejny film",
    "do zobaczenia w następnym odcinku",
];

pub(crate) fn normalize(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn normalized_blacklist() -> &'static [String] {
    static L: OnceLock<Vec<String>> = OnceLock::new();
    L.get_or_init(|| BLACKLIST.iter().map(|p| normalize(p)).collect())
}

/// Jak długo pamiętać ostatnią zaakceptowaną frazę do deduplikacji —
/// prawdziwe pętle dekodera whispera powtarzają się w skali sekund, nie
/// minut; dłuższe okno zaczęłoby gubić legalne, powtórzone w dialogu kwestie
/// ("Yes." ... "Yes.").
const DEDUP_WINDOW: Duration = Duration::from_secs(10);
/// poniżej tej długości (w znakach) nigdy nie deduplikujemy — krótkie
/// odpowiedzi ("tak", "no", "okay") są zbyt częste, żeby dowolne powtórzenie
/// uznawać za halucynację
const DEDUP_MIN_CHARS: usize = 12;

/// Filtr warstwowy: progi pewności, czarna lista fraz, powtórzenia z rzędu.
/// Halucynacje bywają "pewne" (wysoki logprob, niski no_speech), stąd lista.
pub struct HallucinationFilter {
    last_seen: Option<(String, Instant)>,
}

impl HallucinationFilter {
    pub fn new() -> Self {
        Self { last_seen: None }
    }

    /// Zwraca `Some(powód)`, gdy segment należy odrzucić — jawny powód
    /// zamiast gołego boola, żeby dało się to zdiagnozować bez zgadywania
    /// (wcześniej odrzucenia znikały bez śladu na domyślnym poziomie logu).
    pub fn reject_reason(&mut self, t: &Transcript) -> Option<String> {
        if t.text.is_empty() {
            return Some("pusty tekst".into());
        }
        if t.no_speech_prob > 0.6 && t.avg_logprob < -1.0 {
            return Some(format!(
                "niska pewność (no_speech={:.2}, logprob={:.2})",
                t.no_speech_prob, t.avg_logprob
            ));
        }
        let norm = normalize(&t.text);
        if norm.is_empty() {
            return Some("tekst po normalizacji pusty (same znaki specjalne)".into());
        }
        let n_chars = norm.chars().count();
        // dopasowanie przez podłańcuch tylko gdy fraza pokrywa większość
        // segmentu (>=70% długości w znakach) — inaczej legalne zdanie
        // WSPOMINAJĄCE frazę z listy ("he said please subscribe...") ginie
        if let Some(ph) = normalized_blacklist().iter().find(|ph| {
            norm == **ph || (norm.contains(ph.as_str()) && ph.chars().count() * 10 >= n_chars * 7)
        }) {
            return Some(format!("pasuje do czarnej listy halucynacji (\"{ph}\")"));
        }
        // identyczne powtórzenie w krótkim oknie czasu = pętla dekodera;
        // poza oknem albo dla bardzo krótkich fraz to zwykła mowa
        if n_chars > DEDUP_MIN_CHARS {
            if let Some((last, at)) = &self.last_seen {
                let elapsed = at.elapsed();
                if *last == norm && elapsed < DEDUP_WINDOW {
                    self.last_seen = Some((norm, Instant::now()));
                    return Some(format!(
                        "powtórzenie poprzedniego tekstu sprzed {:.1}s (pętla dekodera?)",
                        elapsed.as_secs_f32()
                    ));
                }
            }
        }
        self.last_seen = Some((norm, Instant::now()));
        None
    }
}
