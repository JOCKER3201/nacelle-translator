use anyhow::Context;
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub audio: AudioCfg,
    pub vad: VadCfg,
    pub stt: SttCfg,
    pub translate: MtCfg,
    pub tts: TtsCfg,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AudioCfg {
    /// node.name sprzętowego sinka; nieobecny/zakomentowany lub pusty =
    /// automatyczny wybór pierwszego urządzenia ALSA/Bluetooth
    pub output_device: Option<String>,
    pub passthrough_gain: f32,
    /// głośność oryginału, gdy mówi lektor (0.2 ≈ -14 dB)
    pub duck_gain: f32,
    pub tts_gain: f32,
    pub duck_attack_ms: f32,
    pub duck_release_ms: f32,
    pub duck_hold_ms: f32,
}

impl Default for AudioCfg {
    fn default() -> Self {
        Self {
            output_device: None,
            passthrough_gain: 1.0,
            duck_gain: 0.2,
            tts_gain: 1.0,
            duck_attack_ms: 30.0,
            duck_release_ms: 500.0,
            duck_hold_ms: 250.0,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VadCfg {
    pub threshold_enter: f32,
    pub threshold_exit: f32,
    pub preroll_ms: u32,
    pub min_speech_ms: u32,
    pub hangover_ms: u32,
    /// skrócony hangover obowiązujący, gdy segment przekroczył soft_max_ms —
    /// szybka mowa robi między frazami pauzy za krótkie na pełny hangover;
    /// bez tego wszystko czeka na twarde cięcie przy hard_max_ms
    pub soft_hangover_ms: u32,
    /// ile ms musi minąć od najgłębszego dołka p, żeby uciąć segment w tym
    /// dołku bez czekania na hard_max_ms — dla ciągłej mowy bez żadnych pauz
    /// to główny mechanizm ograniczający opóźnienie pierwszego fragmentu
    pub dip_settle_ms: u32,
    /// próg "wystarczająco dobrego" dołka dla cięcia ustabilizowanego —
    /// celowo LUŹNIEJSZY niż threshold_exit: przy mowie z podkładem
    /// muzycznym Silero nie schodzi poniżej threshold_exit nawet na
    /// granicach fraz, ale dołki 0.4-0.6 tam występują
    pub dip_threshold: f32,
    pub soft_max_ms: u32,
    pub hard_max_ms: u32,
    pub overlap_ms: u32,
}

impl Default for VadCfg {
    fn default() -> Self {
        Self {
            threshold_enter: 0.5,
            threshold_exit: 0.35,
            preroll_ms: 300,
            min_speech_ms: 500,
            hangover_ms: 800,
            soft_hangover_ms: 250,
            dip_settle_ms: 500,
            dip_threshold: 0.6,
            soft_max_ms: 3_000,
            hard_max_ms: 8_000,
            overlap_ms: 250,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SttCfg {
    /// model ggml WIELOJĘZYCZNY (nie *.en)
    pub model: String,
    /// "auto" albo kod języka źródłowego, np. "en"
    pub language: String,
    pub threads: i32,
}

impl Default for SttCfg {
    fn default() -> Self {
        Self {
            model: "models/ggml-small.bin".into(),
            language: "auto".into(),
            threads: 8,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MtCfg {
    /// "gemini" (domyślny, szybkie API w chmurze) | "ollama" (lokalny model,
    /// zbyt wolny do czasu rzeczywistego przy dużych modelach) | "llamacpp"
    /// (lokalny llama-server z modelem GGUF na GPU) | "claude" (API) |
    /// "off" (test toru bez tłumaczenia)
    pub engine: String,
    /// model dla silnika "claude"
    pub model: String,
    /// model dla silnika "gemini" (Interactions API)
    pub gemini_model: String,
    /// adres serwera Ollama
    pub ollama_host: String,
    /// nazwa/tag modelu w Ollamie (np. lokalny alias na Gemma 4 Q4)
    pub ollama_model: String,
    /// adres serwera llama-server (surowy endpoint /completion; serwer
    /// serwuje jeden wczytany model GGUF, uruchamiany z --no-jinja)
    pub llamacpp_host: String,
    /// nazwa języka używana w prompcie
    pub target_language: String,
    /// kod języka do porównania z detekcją whispera
    pub target_lang_code: String,
    /// nie tłumacz/nie czytaj, gdy wykryty język == docelowy
    pub skip_target_lang: bool,
    /// ile poprzednich par (oryginał → tłumaczenie) trzymać w kontekście
    /// (dla "gemini" to też próg, po którym łańcuch previous_interaction_id
    /// jest resetowany, żeby kontekst nie rósł bez końca)
    pub context_pairs: usize,
    /// limit długości odpowiedzi (Claude: max_tokens; Ollama: num_predict;
    /// Gemini: max_output_tokens)
    pub max_tokens: u32,
    /// timeout wywołania dla silników "claude" i "gemini" (szybkie API)
    pub timeout_s: u64,
    /// timeout wywołania dla silnika "ollama" — lokalny model (dziesiątki
    /// GB, CPU/GPU dzielone z resztą systemu) bywa wolniejszy niż API
    pub ollama_timeout_s: u64,
}

impl Default for MtCfg {
    fn default() -> Self {
        Self {
            engine: "gemini".into(),
            model: "claude-opus-4-8".into(),
            gemini_model: "gemini-3.5-flash-lite".into(),
            ollama_host: "http://localhost:11434".into(),
            ollama_model: "gemma".into(),
            llamacpp_host: "http://localhost:8080".into(),
            target_language: "polski".into(),
            target_lang_code: "pl".into(),
            skip_target_lang: true,
            context_pairs: 3,
            max_tokens: 2048,
            timeout_s: 15,
            ollama_timeout_s: 60,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TtsCfg {
    pub piper_bin: String,
    pub voice: String,
    /// <1.0 = szybsza mowa (odwrotność "speed")
    pub length_scale: f32,
    /// tempo lektora w trybie DOGANIANIA — używane, gdy zadanie ma już
    /// sporą zaległość (rozwlekłe tłumaczenie wydłużyło kolejkę): lektor
    /// chwilowo przyspiesza i nadgania, zamiast pozwalać zaległości dobić
    /// do budżetu porzucania. Ustaw równe length_scale, żeby wyłączyć.
    pub catchup_length_scale: f32,
    pub sentence_silence: f32,
    /// katalog roboczy na WAV-y pipera; tmpfs = zero realnego I/O
    pub work_dir: String,
}

impl Default for TtsCfg {
    fn default() -> Self {
        Self {
            piper_bin: "~/.local/share/piper/piper/piper".into(),
            voice: "~/.local/share/piper/pl_PL-gosia-medium.onnx".into(),
            length_scale: 0.9,
            catchup_length_scale: 0.55,
            sentence_silence: 0.1,
            work_dir: "/dev/shm/nacelle-translator-tts".into(),
        }
    }
}

pub fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return Path::new(&home).join(rest);
        }
    }
    PathBuf::from(path)
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        if path.exists() {
            let raw = std::fs::read_to_string(path)
                .with_context(|| format!("nie mogę odczytać {}", path.display()))?;
            let cfg: Config = toml::from_str(&raw)
                .with_context(|| format!("błąd składni w {}", path.display()))?;
            Ok(cfg)
        } else {
            log::info!(
                "brak pliku {} — używam domyślnej konfiguracji",
                path.display()
            );
            Ok(Config::default())
        }
    }

    pub fn piper_bin(&self) -> PathBuf {
        expand_tilde(&self.tts.piper_bin)
    }

    pub fn piper_voice(&self) -> PathBuf {
        expand_tilde(&self.tts.voice)
    }

    pub fn stt_model(&self) -> PathBuf {
        expand_tilde(&self.stt.model)
    }
}
