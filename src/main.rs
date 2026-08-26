//! nacelle-translator — węzeł PipeWire tłumaczący w locie dźwięk w drodze do
//! urządzenia wyjściowego: whisper.cpp (STT) → API Claude (MT) → piper (TTS),
//! z przyciszanym oryginałem pod głosem lektora (ducking).
//!
//! Podkomendy: run (domyślna), devices, check.

mod agreement;
mod audio;
mod config;
mod experimental;
mod pipeline;
mod pw;
mod stt;
mod translate;
mod tts;
mod vad;

use anyhow::Result;
use config::Config;
use ringbuf::{traits::*, HeapRb};
use std::path::PathBuf;

const USAGE: &str = "\
nacelle-translator — tłumacz audio w locie (węzeł PipeWire)

Użycie:
  nacelle-translator [run] [--config PLIK] [-v]   uruchom translator
  nacelle-translator devices                      wypisz węzły Audio/Sink
  nacelle-translator check [--config PLIK]        sprawdź konfigurację i zależności

Opcje:
  --config PLIK   ścieżka do nacelle-translator.toml (domyślnie ./nacelle-translator.toml)
  -v, --verbose   więcej logów (poziom debug)
";

/// Pełna pomoc = stała USAGE + lista opcji eksperymentalnych generowana
/// z tablicy w experimental.rs (dopisanie opcji nie wymaga ruszania USAGE).
fn usage() -> String {
    format!("{USAGE}{}", experimental::help_block())
}

/// Wyjście z kodem 2 (zły wiersz poleceń) — odróżnialne od kodu 1, którym
/// kończy się błąd działania już uruchomionego programu.
fn die_usage(msg: &str) -> ! {
    eprintln!("{msg}\n\n{}", usage());
    std::process::exit(2);
}

fn main() {
    let mut cmd = String::from("run");
    let mut config_path = PathBuf::from("nacelle-translator.toml");
    let mut verbose = false;
    let mut experimental = experimental::Selection::default();

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "run" | "devices" | "check" => cmd = a,
            "--config" => match args.next() {
                Some(p) => config_path = PathBuf::from(p),
                None => die_usage("--config wymaga ścieżki"),
            },
            // Obie formy zapisu: "=LISTA" (czytelna, bo widać, że wartość
            // należy do flagi) i "FLAGA LISTA" (jak --config PLIK, więc nie
            // zaskakuje). Rozdzielenie ich byłoby czystym utrudnieniem.
            s if s.starts_with(&format!("{}=", experimental::FLAG)) => {
                let spec = &s[experimental::FLAG.len() + 1..];
                if let Err(e) = experimental.extend_from_spec(spec) {
                    die_usage(&e);
                }
            }
            s if s == experimental::FLAG => match args.next() {
                Some(spec) => {
                    if let Err(e) = experimental.extend_from_spec(&spec) {
                        die_usage(&e);
                    }
                }
                None => die_usage(&format!("{} wymaga listy opcji", experimental::FLAG)),
            },
            "-v" | "--verbose" => verbose = true,
            "-h" | "--help" => {
                print!("{}", usage());
                return;
            }
            other => die_usage(&format!("nieznany argument: {other}")),
        }
    }

    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or(if verbose { "debug" } else { "info" }),
    )
    .format_timestamp_millis()
    .init();

    let result = match cmd.as_str() {
        "devices" => cmd_devices(),
        "check" => cmd_check(&config_path, &experimental),
        _ => cmd_run(&config_path, &experimental),
    };
    if let Err(e) = result {
        log::error!("{e:#}");
        std::process::exit(1);
    }
}

fn cmd_run(config_path: &PathBuf, experimental: &experimental::Selection) -> Result<()> {
    let mut cfg = Config::load(config_path)?;
    // opcje eksperymentalne nakładamy na WCZYTANĄ konfigurację — dalej cały
    // tor AI widzi zwykły Config i nie zna pojęcia flagi
    experimental.apply(&mut cfg);
    experimental.log_startup();

    // ringbuffery RT ↔ tor AI
    let (cap_prod, cap_cons) = HeapRb::<f32>::new(pw::RATE as usize * 4).split(); // mono, 4 s
    let (mut pass_prod, pass_cons) =
        HeapRb::<f32>::new(pw::RATE as usize * pw::CHANNELS * 2).split(); // stereo, 2 s
    // Ring TTS to TWARDY limit zaległości lektora względem oryginału: wątek
    // TTS wpycha klipy przyrostowo (pipeline.rs), więc klip dłuższy niż ring
    // też przejdzie — ale nowa mowa lektora zacznie się najpóźniej ~5 s po
    // zsyntetyzowaniu. Większy ring = dłuższe "doganianie" po serii
    // rozwlekłych tłumaczeń (polski bywa ~10% dłuższy od angielskiego).
    let (tts_prod, tts_cons) = HeapRb::<f32>::new(pw::RATE as usize * 3).split(); // mono, 3 s

    // luz na start toru passthrough (~85 ms), żeby odtwarzanie nie łapało
    // underrunów zanim sink zacznie produkować
    let prime = vec![0.0f32; 4096 * pw::CHANNELS];
    let _ = pass_prod.push_slice(&prime);

    let health_rx = pipeline::spawn(cfg.clone(), cap_cons, tts_prod)?;

    pw::run_graph(
        cfg.audio.output_device.as_deref(),
        pw::RtRings {
            cap_prod,
            pass_prod,
            pass_cons,
            tts_cons,
        },
        pw::DuckParams::from_cfg(&cfg.audio),
        health_rx,
    )
}

fn cmd_devices() -> Result<()> {
    let (sinks, default) = pw::discover_sinks()?;
    if sinks.is_empty() {
        println!("brak węzłów Audio/Sink");
        return Ok(());
    }
    println!("{:>5}  {:<7} {:<4} {:<60} opis", "id", "sprzęt", "dom.", "node.name");
    for s in &sinks {
        let hw = if s.name.starts_with("alsa_output.") || s.name.starts_with("bluez_output.") {
            "tak"
        } else {
            "-"
        };
        let is_default = if default.as_deref() == Some(s.name.as_str()) { "*" } else { "" };
        println!(
            "{:>5}  {:<7} {:<4} {:<60} {}",
            s.id, hw, is_default, s.name, s.description
        );
    }
    Ok(())
}

fn check_item(failures: &mut usize, cond: bool, msg_ok: String, msg_err: String) {
    if cond {
        println!("  OK    {msg_ok}");
    } else {
        println!("  BŁĄD  {msg_err}");
        *failures += 1;
    }
}

fn cmd_check(config_path: &PathBuf, experimental: &experimental::Selection) -> Result<()> {
    let mut failures = 0usize;

    println!("konfiguracja: {}", config_path.display());
    let mut cfg = match Config::load(config_path) {
        Ok(c) => c,
        Err(e) => {
            println!("  BŁĄD  {e:#}");
            std::process::exit(1);
        }
    };
    if config_path.exists() {
        println!("  OK    składnia poprawna");
    } else {
        println!("  OK    brak pliku — używam domyślnej konfiguracji");
    }

    // Opcje eksperymentalne zawsze jako OK (nieznana nazwa nie dochodzi tu
    // wcale — parser kończy proces kodem 2), ale WYPISANE, żeby `check`
    // pokazywał ten sam tor, którym pojedzie `run` z tymi samymi argumentami.
    experimental.apply(&mut cfg);
    if experimental.is_empty() {
        println!("  OK    opcje eksperymentalne: brak (włącza je {}=…)", experimental::FLAG);
    } else {
        println!("  OK    opcje eksperymentalne: {}", experimental.names().join(", "));
    }

    // model whisper
    let model = cfg.stt_model();
    check_item(
        &mut failures,
        model.exists(),
        format!("model whisper: {}", model.display()),
        format!(
            "brak modelu whisper: {} — pobierz np.:\n        curl -L --create-dirs -o {} \\\n          https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small.bin",
            model.display(),
            model.display()
        ),
    );
    let name = model.file_name().unwrap_or_default().to_string_lossy().to_string();
    check_item(
        &mut failures,
        // ".en." łapie ggml-base.en.bin; ".en-" łapie kwantyzowane warianty
        // (ggml-small.en-q5_1.bin) — bez tego drugiego warunku przechodziły
        // jako "wielojęzyczne"
        !(name.contains(".en.") || name.contains(".en-")),
        "model wielojęzyczny".into(),
        format!(
            "model \"{name}\" jest angielsko-tylko (*.en / *.en-q*) — do tłumaczenia \
             potrzebny model wielojęzyczny (ggml-small.bin, ggml-base.bin, ...)"
        ),
    );

    // piper
    let piper_bin = cfg.piper_bin();
    let voice = cfg.piper_voice();
    check_item(
        &mut failures,
        piper_bin.exists(),
        format!("piper: {}", piper_bin.display()),
        format!("brak binarki pipera: {}", piper_bin.display()),
    );
    check_item(
        &mut failures,
        voice.exists(),
        format!("głos: {}", voice.display()),
        format!("brak głosu pipera: {}", voice.display()),
    );
    match tts::read_voice_sample_rate(&voice) {
        Ok(rate) => println!("  OK    config głosu: {rate} Hz"),
        Err(e) => {
            println!("  BŁĄD  config głosu: {e:#}");
            failures += 1;
        }
    }
    check_item(
        &mut failures,
        std::fs::create_dir_all(&cfg.tts.work_dir).is_ok(),
        format!("katalog roboczy TTS: {}", cfg.tts.work_dir),
        format!("nie mogę utworzyć katalogu roboczego TTS: {}", cfg.tts.work_dir),
    );

    // silnik tłumaczenia — dopasowanie musi pokrywać dokładnie to, co
    // akceptuje make_translator, inaczej check przepuszcza literówkę
    // (np. "claud"), a `run` pada dopiero przy starcie pipeline'u
    match cfg.translate.engine.as_str() {
        "gemini" => check_item(
            &mut failures,
            std::env::var("GEMINI_API_KEY")
                .map(|v| !v.trim().is_empty())
                .unwrap_or(false),
            format!("GEMINI_API_KEY ustawiony (model: {})", cfg.translate.gemini_model),
            "brak (albo pusta) zmienna środowiskowa GEMINI_API_KEY".into(),
        ),
        "ollama" => match translate::ollama_check(&cfg.translate.ollama_host, &cfg.translate.ollama_model) {
            Ok(()) => println!(
                "  OK    Ollama: {} ma model \"{}\"",
                cfg.translate.ollama_host, cfg.translate.ollama_model
            ),
            Err(e) => {
                println!("  BŁĄD  {e:#}");
                failures += 1;
            }
        },
        "llamacpp" => match translate::llamacpp_check(&cfg.translate.llamacpp_host) {
            Ok(()) => println!("  OK    llama-server: {} odpowiada", cfg.translate.llamacpp_host),
            Err(e) => {
                println!("  BŁĄD  {e:#}");
                failures += 1;
            }
        },
        "claude" => check_item(
            &mut failures,
            std::env::var("ANTHROPIC_API_KEY")
                .map(|v| !v.trim().is_empty())
                .unwrap_or(false),
            format!("ANTHROPIC_API_KEY ustawiony (model: {})", cfg.translate.model),
            "brak (albo pusta) zmienna środowiskowa ANTHROPIC_API_KEY".into(),
        ),
        "off" => println!("  OK    silnik tłumaczenia: off (bez API)"),
        other => {
            println!(
                "  BŁĄD  nieznany silnik tłumaczenia: {other} (dozwolone: gemini, ollama, llamacpp, claude, off)"
            );
            failures += 1;
        }
    }

    // pipewire
    match pw::discover_sinks() {
        Ok((sinks, default)) => {
            println!("  OK    PipeWire: {} węzłów Audio/Sink", sinks.len());
            match default.as_deref() {
                Some(name) => println!("  OK    aktualne domyślne wyjście (odczyt): {name}"),
                None => println!("  OK    aktualne domyślne wyjście: nie udało się odczytać (użyję heurystyki)"),
            }
            match pw::pick_output(&sinks, cfg.audio.output_device.as_deref(), default.as_deref()) {
                Ok(t) => println!("  OK    cel odtwarzania: {} ({})", t.name, t.description),
                Err(e) => {
                    println!("  BŁĄD  {e:#}");
                    failures += 1;
                }
            }
        }
        Err(e) => {
            println!("  BŁĄD  PipeWire: {e:#}");
            failures += 1;
        }
    }

    if failures > 0 {
        println!("\n{failures} problem(ów) do rozwiązania");
        std::process::exit(1);
    }
    println!("\nwszystko gotowe — uruchom: nacelle-translator run");
    Ok(())
}
