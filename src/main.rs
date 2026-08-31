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

/// Blokuje SIGINT/SIGTERM na wątku głównym, ZANIM cokolwiek — w tym
/// `pipeline::spawn` w `cmd_run` — zdąży stworzyć choć jeden wątek.
///
/// Wątek dziedziczy maskę sygnałów wątku, który go tworzy, w CHWILI
/// tworzenia — maska jest per-wątek, nie per-proces. `pw::run_graph`
/// rejestruje własną obsługę tych sygnałów (`add_signal_local`), ale robi to
/// PÓŹNO: dopiero w środku, długo po tym jak `pipeline::spawn` uruchomił
/// wątki toru AI (STT, MT, TTS, segmenter, watchdog). Te wątki dziedziczyły
/// więc maskę sprzed jakiejkolwiek blokady — sygnał wysłany do procesu
/// (`kill`, Ctrl+C) mógł trafić w kernelu w DOWOLNY z nich, a żaden nie ma
/// własnego handlera, więc zabijał cały proces przez domyślną dyspozycję:
/// zero logu, zero sprzątania, ścieżka `pw::run_graph`'s `Ok(())` nigdy nie
/// uruchomiona. Zweryfikowane empirycznie: `kill -TERM`/`kill -INT` na PID
/// żywego procesu kończyły go natychmiast, bez śladu w logu.
///
/// Blokada tu, zanim jakikolwiek wątek istnieje, sprawia że KAŻDY później
/// tworzony wątek (co najmniej `std::thread::spawn`, który na Linuksie idzie
/// przez `pthread_create`) dziedziczy już zablokowaną maskę — jedynym
/// miejscem, które w ogóle może odebrać sygnał, zostaje mechanizm PipeWire.
/// Redundancja z tym, co `add_signal_local` i tak robi wewnętrznie na wątku
/// głównym, jest nieszkodliwa (blokowanie już zablokowanego sygnału to no-op).
///
/// Sygnał wysłany w oknie między tą funkcją a rejestracją w `run_graph` NIE
/// ginie — zablokowany sygnał staje się PENDING dla procesu i zostanie
/// odebrany, gdy tylko `add_signal_local` zacznie go nasłuchiwać.
///
/// Procesów potomnych (piper, uruchamiany przez `Command::spawn`) to nie
/// dotyczy w praktyce: piper kończy się sam po EOF na stdin, gdy proces
/// nadrzędny umiera, a jego jawne sprzątanie w `PiperTts::drop` idzie przez
/// `Child::kill()` (SIGKILL) — niemaskowalny, działa niezależnie od maski
/// sygnałów dziedziczonej przez fork+exec.
fn block_shutdown_signals_before_spawning_threads() {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGINT);
        libc::sigaddset(&mut set, libc::SIGTERM);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
    }
}

fn main() {
    block_shutdown_signals_before_spawning_threads();
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
    // strojenie sprawdzamy PO nałożeniu opcji — inaczej nie wiadomo, którego
    // toru dotyczy wartość z pliku
    if let Some(w) = cfg.tuning_warning() {
        log::warn!("{w}");
    }
    if let Some(w) = cfg.output_device_warning() {
        log::warn!("{w}");
    }

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

    // BRAMKA MUSI ZADZIAŁAĆ TUTAJ, nie dopiero w callbackach RT. `pipeline::
    // spawn` ładuje wagi whispera do VRAM, buduje tłumacza (przy silniku
    // `gemini`/`claude` czyta klucz API i BAILUJE, gdy go nie ma; przy
    // `ollama` robi pełną rozgrzewkową inferencję) i odpala dwa procesy
    // pipera. Wołane bezwarunkowo sprawiało, że domyślna konfiguracja
    // (`translate = false`) NIE WSTAWAŁA na świeżym klonie: `?` leciało do
    // `main` i kończyło proces kodem 1, zanim w ogóle powstał węzeł
    // PipeWire. Czyli tryb reklamowany jako „czysta przelotka, GPU stoi"
    // wymagał modelu za 488 MB, binarki pipera i klucza API.
    //
    // `chan::never()` ma dokładnie typ `Receiver<String>` i nigdy nic nie
    // przysyła ani się nie rozłącza, więc wątek-dozorca w `run_graph`
    // (`health_rx.recv()`) po prostu blokuje się do końca życia procesu —
    // a nie widzi rozłączonego kanału i nie melduje fałszywej śmierci toru.
    // `cap_cons` i `tts_prod` są wtedy porzucane: przy zamkniętej bramce
    // nikt do tych ringów nie pisze ani z nich nie czyta.
    let health_rx = if cfg.audio.translate {
        pipeline::spawn(cfg.clone(), cap_cons, tts_prod)?
    } else {
        log::info!(
            "tor AI NIE JEST budowany ([audio].translate = false): whisper nie dotyka GPU, \
             piper się nie uruchamia, silnik tłumaczenia nie jest potrzebny"
        );
        crossbeam_channel::never::<String>()
    };

    pw::run_graph(
        pw::RtRings {
            cap_prod,
            pass_prod,
            pass_cons,
            tts_cons,
        },
        pw::DuckParams::from_cfg(&cfg.audio),
        pw::TranslateGate::new(cfg.audio.translate),
        health_rx,
    )
}

fn cmd_devices() -> Result<()> {
    let pw::GraphSnapshot { sinks, defaults: default, .. } = pw::discover_sinks()?;
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
        // gwiazdka przy wyjściu AKTYWNYM (tym, po które WirePlumber sięga przy
        // linkowaniu), a nie przy zapamiętanym wyborze — ten drugi potrafi
        // wskazywać urządzenie, którego w tej chwili nie ma w grafie
        let is_default = if default.effective() == Some(s.name.as_str()) { "*" } else { "" };
        println!(
            "{:>5}  {:<7} {:<4} {:<60} {}",
            s.id, hw, is_default, s.name, s.description
        );
    }
    Ok(())
}

/// Waga braku zależności toru AI zależy od bramki.
///
/// Przy `[audio].translate = false` tor AI w ogóle nie powstaje (cmd_run
/// pomija `pipeline::spawn`), więc brak modelu whispera, pipera czy klucza API
/// NIE przeszkadza w uruchomieniu przelotki. Zgłaszanie tego jako BŁĄD dawało
/// kod wyjścia 1 dla konfiguracji, którą `check` dwie linie wyżej sam nazywał
/// poprawną. Wypisujemy więc UWAGA i nie ruszamy licznika — informacja
/// zostaje, werdykt się zmienia.
struct AiSeverity {
    fatal: bool,
}

impl AiSeverity {
    fn item(&self, failures: &mut usize, cond: bool, msg_ok: String, msg_err: String) {
        if cond {
            println!("  OK    {msg_ok}");
        } else if self.fatal {
            println!("  BŁĄD  {msg_err}");
            *failures += 1;
        } else {
            println!("  UWAGA {msg_err}\n        (tłumaczenie wyłączone, więc to nie blokuje startu — przelotka ruszy bez tego)");
        }
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
    // UWAGA, nie BŁĄD: konfiguracja jest poprawna, tylko dostrojona pod drugi
    // tor — nie ma powodu, żeby przez to zwracać kod wyjścia 1
    if let Some(w) = cfg.tuning_warning() {
        println!("  UWAGA {w}");
    }
    if let Some(w) = cfg.output_device_warning() {
        println!("  UWAGA {w}");
    }

    // Bramka toru AI — wypisana WYSOKO, bo bez niej `check` potrafi
    // wyświetlić same OK komuś, kto potem nie usłyszy ani słowa lektora
    // (model, piper i klucz API są sprawne; po prostu nic ich nie woła).
    // To nie jest BŁĄD: przelotka bez tłumaczenia jest poprawnym trybem.
    let ai = AiSeverity { fatal: cfg.audio.translate };
    if cfg.audio.translate {
        println!("  OK    tłumaczenie WŁĄCZONE ([audio].translate = true)");
    } else {
        println!(
            "  OK    tłumaczenie WYŁĄCZONE (domyślnie) — węzeł jest czystą przelotką: tor AI \
             w ogóle się nie buduje, więc model whispera, piper i silnik tłumaczenia nie są \
             potrzebne.\n        Włącz wpisem `translate = true` w sekcji [audio]."
        );
    }

    // model whisper
    let model = cfg.stt_model();
    ai.item(
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
    ai.item(
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
    ai.item(
        &mut failures,
        piper_bin.exists(),
        format!("piper: {}", piper_bin.display()),
        format!("brak binarki pipera: {}", piper_bin.display()),
    );
    ai.item(
        &mut failures,
        voice.exists(),
        format!("głos: {}", voice.display()),
        format!("brak głosu pipera: {}", voice.display()),
    );
    match tts::read_voice_sample_rate(&voice) {
        Ok(rate) => println!("  OK    config głosu: {rate} Hz"),
        Err(e) => ai.item(&mut failures, false, String::new(), format!("config głosu: {e:#}")),
    }
    ai.item(
        &mut failures,
        std::fs::create_dir_all(&cfg.tts.work_dir).is_ok(),
        format!("katalog roboczy TTS: {}", cfg.tts.work_dir),
        format!("nie mogę utworzyć katalogu roboczego TTS: {}", cfg.tts.work_dir),
    );

    // silnik tłumaczenia — dopasowanie musi pokrywać dokładnie to, co
    // akceptuje make_translator, inaczej check przepuszcza literówkę
    // (np. "claud"), a `run` pada dopiero przy starcie pipeline'u
    match cfg.translate.engine.as_str() {
        "gemini" => ai.item(
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
            Err(e) => ai.item(&mut failures, false, String::new(), format!("{e:#}")),
        },
        "llamacpp" => match translate::llamacpp_check(&cfg.translate.llamacpp_host) {
            Ok(()) => println!("  OK    llama-server: {} odpowiada", cfg.translate.llamacpp_host),
            Err(e) => ai.item(&mut failures, false, String::new(), format!("{e:#}")),
        },
        "claude" => ai.item(
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
        Ok(pw::GraphSnapshot { sinks, defaults: default, session_manager }) => {
            println!("  OK    PipeWire: {} węzłów Audio/Sink", sinks.len());
            // Bez WirePlumbera `filter.smart` nie znaczy NIC: węzeł powstanie
            // i zostanie niezlinkowany. To jedyna twarda zależność runtime,
            // której `check` wcześniej w ogóle nie sprawdzał — i drukował
            // zielone OK komuś, kto potem nie usłyszy nic.
            match session_manager.as_deref() {
                Some(n) if n.starts_with("WirePlumber") => {
                    println!("  OK    menedżer sesji: {n} (realizuje filter.smart)")
                }
                Some(n) => println!(
                    "  UWAGA menedżer sesji to \"{n}\", a nie WirePlumber — `filter.smart` \
                     jest polityką WirePlumbera >= 0.5.\n        Bez niego węzeł powstanie, \
                     ale nikt go nie zlinkuje i translator będzie niemy."
                ),
                None => println!(
                    "  UWAGA nie widzę w grafie klienta WirePlumbera. Jeśli menedżerem sesji \
                     nie jest WirePlumber >= 0.5,\n        `filter.smart` nie zadziała i \
                     translator będzie niemy (sprawdź: wireplumber --version)."
                ),
            }
            // OBA klucze osobno. `default.audio.sink` (aktywny) decyduje
            // o routingu, `default.configured.audio.sink` to zapamiętany wybór
            // użytkownika i potrafi wskazywać sprzęt, którego nie ma w grafie.
            // Sklejone w jedno maskowały się nawzajem.
            match default.active.as_deref() {
                Some(name) => println!("  OK    aktywne domyślne wyjście (default.audio.sink): {name}"),
                None => println!("  OK    aktywne domyślne wyjście: nie udało się odczytać"),
            }
            match default.configured.as_deref() {
                Some(name) => println!("  OK    zapamiętany wybór (default.configured.audio.sink): {name}"),
                None => println!("  OK    zapamiętany wybór wyjścia: brak"),
            }
            // Celu NIE sprawdzamy — nie mamy go. Wybiera WirePlumber, a my
            // wpinamy się w to, co jest domyślne w danej chwili.
            //
            // UWAGA, nie BŁĄD: gdy domyślnym wyjściem jest nasz węzeł, dźwięk
            // MIMO TO gra. find-default-target.lua nie ustawia celu (canLink
            // odmawia na własnej link-group), ale też nie przerywa
            // przetwarzania, więc find-best-target.lua pomija inteligentne
            // filtry i dopina nas do najlepszego sprzętowego sinka. Poprzednia
            // wersja zwracała tu kod wyjścia 1 za stan, który działa.
            let us_active = default.active.as_deref() == Some(pw::SINK_NODE_NAME);
            let us_configured = default.configured.as_deref() == Some(pw::SINK_NODE_NAME);
            if us_active || us_configured {
                let które = match (us_active, us_configured) {
                    (true, true) => "aktywnym domyślnym wyjściem I zapamiętanym wyborem",
                    (true, false) => "aktywnym domyślnym wyjściem",
                    _ => "zapamiętanym wyborem wyjścia",
                };
                println!(
                    "  UWAGA {} jest sam translator ({}) — dźwięk będzie grał (WirePlumber \
                     dopnie nasz strumień do sprzętu przez find-best-target), ale to \
                     ustawienie jest mylące.\n        Translator jest przelotką, nie \
                     urządzeniem: wybierz w Ustawieniach systemowych → Dźwięk swój prawdziwy \
                     sprzęt, a translator wpnie się w tor sam.",
                    które,
                    pw::SINK_NODE_NAME
                );
            } else {
                println!("  OK    translator wepnie się w aktualne domyślne wyjście (filter.smart)");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn g1_brak_zaleznosci_ai_nie_wywala_check_gdy_bramka_zamknieta() {
        // Regresja, którą wypuściła poprzednia wersja: `check` przy
        // `translate = false` reklamował „czystą przelotkę", a dwie linie
        // niżej liczył brak modelu whispera i klucza API jako BŁĄD i kończył
        // kodem 1. Skoro tor AI się wtedy nie buduje (cmd_run), brak jego
        // zależności nie może być powodem do niezerowego kodu wyjścia.
        let mut f = 0usize;
        AiSeverity { fatal: false }.item(&mut f, false, String::new(), "brak modelu".into());
        assert_eq!(f, 0, "zamknięta bramka nie może podbijać licznika błędów");
    }

    #[test]
    fn g2_brak_zaleznosci_ai_dalej_jest_bledem_gdy_bramka_otwarta() {
        // Druga strona tego samego niezmiennika: przy WŁĄCZONYM tłumaczeniu
        // brak modelu to nadal twardy błąd — inaczej `check` przepuszczałby
        // konfigurację, na której `run` padnie przy starcie pipeline'u.
        let mut f = 0usize;
        AiSeverity { fatal: true }.item(&mut f, false, String::new(), "brak modelu".into());
        assert_eq!(f, 1);
        // spełniony warunek nigdy nie podbija licznika, niezależnie od wagi
        AiSeverity { fatal: true }.item(&mut f, true, "jest".into(), String::new());
        assert_eq!(f, 1);
    }
}
