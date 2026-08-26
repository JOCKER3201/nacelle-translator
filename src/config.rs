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
    /// NIEZMIENNIK: `hard_max_ms - soft_max_ms >= 2000`.
    ///
    /// Cięcie w ustabilizowanym dołku może odpalić NAJWCZEŚNIEJ w chwili
    /// soft_max_ms — dołki są śledzone dopiero od `soft_max - dip_settle`
    /// (vad.rs), a `settled_dip` wymaga jeszcze `dip_settle_ms` bez głębszego
    /// dołka. Okno wyścigu "dołek zdąży przed twardym cięciem" to więc
    /// DOKŁADNIE ta różnica, i zejście poniżej niej zamienia cięcia w dołku
    /// (dobra granica frazy) na cięcia twarde (w połowie słowa).
    /// Zmierzony w audycie rozkład czekania na ustabilizowany dołek, liczony
    /// od soft_max: mediana 0.65 s, p90 1.33 s, p95 1.60 s, max 1.70 s —
    /// czyli 2.0 s to p95 + 0.4 s marginesu i daje obserwowane 6 % domknięć
    /// przez hard-max. Zwężenie okna: 1.5 s -> ~11 % hard-max, 1.25 s -> ~20 %,
    /// 1.0 s -> ~23-26 %.
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
    /// spekulacyjne STT: whisper puszczany co cadence_ms CZASU AUDIO na
    /// rosnącym, otwartym segmencie; stabilny prefiks (LocalAgreement-2)
    /// idzie do tłumaczenia od razu, final domyka tylko ogon.
    ///
    /// POLE WYŁĄCZNIE RUNTIME — celowo `skip`, czyli NIE do ustawienia z pliku
    /// TOML: jedynym wejściem jest flaga `--experimental-futures=speculative-stt`
    /// (patrz experimental.rs), którą main.rs nakłada na wczytaną konfigurację.
    /// Dzięki temu tor AI czyta dalej po prostu `stt_cfg.speculative` i nie musi
    /// wiedzieć, skąd ta wartość pochodzi.
    #[serde(skip)]
    pub speculative: bool,
    /// kadencja przebiegów częściowych [ms czasu audio] — liczona chunkami
    /// segmentera (32 ms), nie zegarem ściennym: zegar ścienny kłamie przy
    /// wstrzykiwanej ciszy i zaległościach resamplera
    pub cadence_ms: u32,
    /// poniżej tej długości otwartego bufora nie spekulujemy: pad zerowy
    /// whispera (MIN_SAMPLES = 1,1 s) daje na krótkim audio skorelowane
    /// halucynacje, które LocalAgreement błędnie uznałby za stabilne
    pub min_open_ms: u32,
    /// minimalna długość fragmentu w znakach (do interpunkcji frazowej);
    /// krótsze fragmenty czekają na kolejny przebieg
    pub min_fragment_chars: usize,
}

impl Default for SttCfg {
    fn default() -> Self {
        Self {
            model: "models/ggml-small.bin".into(),
            language: "auto".into(),
            threads: 8,
            speculative: false,
            cadence_ms: 400,
            min_open_ms: 1500,
            min_fragment_chars: 12,
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

/// Klucze usunięte z pliku konfiguracyjnego wraz z podpowiedzią, co je
/// zastąpiło. Wszystkie sekcje mają `deny_unknown_fields`, więc stary klucz
/// wywala parsowanie CAŁEGO pliku — bez tej tablicy użytkownik dostaje
/// wyłącznie serdowe "unknown field", które nie mówi, gdzie szukać funkcji.
/// Dopisując tu kolejny wpis pamiętaj, że dopasowanie idzie po tekście błędu
/// serde, więc nazwa musi być dokładna.
const RETIRED_KEYS: &[(&str, &str)] = &[(
    "speculative",
    "spekulacyjne STT jest teraz funkcją eksperymentalną i włącza je WYŁĄCZNIE \
     flaga wiersza poleceń: nacelle-translator --experimental-futures=speculative-stt",
)];

/// Parsowanie z podmianą komunikatu dla wycofanych kluczy (osobno od `load`,
/// żeby dało się to przetestować bez dotykania dysku).
fn parse_str(raw: &str, path: &Path) -> anyhow::Result<Config> {
    match toml::from_str::<Config>(raw) {
        Ok(cfg) => Ok(cfg),
        Err(e) => {
            let msg = e.to_string();
            if let Some((key, hint)) = RETIRED_KEYS
                .iter()
                .find(|(k, _)| msg.contains(&format!("unknown field `{k}`")))
            {
                anyhow::bail!(
                    "{}: klucz `{key}` został wycofany z pliku konfiguracyjnego — {hint}\n\
                     usuń linię z `{key}` z pliku i podaj flagę przy uruchomieniu",
                    path.display()
                );
            }
            Err(anyhow::Error::new(e))
                .with_context(|| format!("błąd składni w {}", path.display()))
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        if path.exists() {
            let raw = std::fs::read_to_string(path)
                .with_context(|| format!("nie mogę odczytać {}", path.display()))?;
            parse_str(&raw, path)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wycofany_klucz_daje_komunikat_kierujacy_na_flage() {
        let err = parse_str("[stt]\nspeculative = true\n", Path::new("nacelle-translator.toml"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("--experimental-futures=speculative-stt"), "{err}");
        assert!(err.contains("speculative"), "{err}");
        assert!(!err.contains("unknown field"), "surowy błąd serde nie może wyciec: {err}");
    }

    #[test]
    fn zwykly_blad_skladni_zostaje_bledem_skladni() {
        let err = parse_str("[stt\n", Path::new("x.toml")).unwrap_err().to_string();
        assert!(err.contains("błąd składni"), "{err}");
    }

    #[test]
    fn spekulacja_nie_jest_deserializowana_z_pliku() {
        // reszta sekcji [stt] ma się dalej wczytywać normalnie
        let cfg = parse_str("[stt]\nthreads = 4\n", Path::new("x.toml")).unwrap();
        assert_eq!(cfg.stt.threads, 4);
        assert!(!cfg.stt.speculative);
    }
}
