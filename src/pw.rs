//! Graf PipeWire: wirtualny sink (media.class=Audio/Sink) + strumień
//! odtwarzania wycelowany w sprzętowe urządzenie. Callbacki RT wymieniają
//! próbki z resztą programu wyłącznie przez lock-free ringbuffery SPSC.
//!
//! Ochrona przed pętlą (gdy nasz sink jest domyślnym urządzeniem). Opis stanu
//! FAKTYCZNEGO, sprawdzonego w źródłach WirePlumbera 0.5.12 na tej maszynie —
//! nie deklaracji:
//!  1. `target.object` = node.name sprzętu RAZEM z `node.dont-fallback=true`.
//!     Sam `target.object` nie blokuje NICZEGO: gdy dopasowanie nie trafi,
//!     linking/find-defined-target.lua:82-94 zostawia `target_picked=false`
//!     i oddaje sterowanie dalej, aż do linking/find-default-target.lua,
//!     czyli do domyślnego sinka — a tym może być NASZ sink. Dopiero
//!     `node.dont-fallback` (czytane w find-defined-target.lua:38 wprost
//!     z właściwości węzła; to klucz WirePlumbera, PipeWire go nie zna)
//!     włącza gałąź :116-127: `sendClientError` + `node:request_destroy()`
//!     + `event:stop_processing()` — czyli ucina jakikolwiek fallback
//!     i zamienia cichy zawis w głośny błąd. Świadomy koszt: przejściowa
//!     nieobecność celu (przełączanie profilu karty, uśpienie słuchawek)
//!     też kończy proces — głośno i z instrukcją, zamiast zostawiać niemego
//!     zombie, którym byliśmy wcześniej.
//!  2. StreamFlags::DONT_RECONNECT — UWAGA, to NIE jest ochrona przed
//!     fallbackiem przy PIERWSZYM linkowaniu (prepare-link.lua sięga wtedy po
//!     domyślny sink tak samo). Działa dopiero po pierwszym udanym
//!     zlinkowaniu i wtedy działa aż za dobrze: prepare-link.lua:72-76
//!     (`if not reconnect and si_flags.was_handled then target = nil;
//!     goto done end`) przeskakuje ZARÓWNO `sendClientError`, JAK
//!     I `node:request_destroy()`. Bez punktu 1 zniknięcie celu daje więc
//!     niemego zombie bez jednego wiersza w logu.
//!  3. `node.link-group` o tej samej wartości na obu strumieniach — jedyny
//!     zamek, który nie zależy od naszych właściwości: linking-utils.lua
//!     `canLinkGroupCheck` odmawia linkowania węzłów o tej samej wartości
//!     i rekurencyjnie (do 8 hopów) wykrywa pętle pośrednie.
//!  4. przy automatycznym wyborze celu odfiltrowujemy własne węzły I każdy
//!     wirtualny sink obcego pochodzenia (akceptujemy tylko sprzęt ALSA/BT).
//!
//! Odporność: oba strumienie rejestrują `state_changed`. Prawdziwy błąd
//! strumienia (`stream.state()` faktycznie w Error) albo przejście
//! w `Unconnected` (serwer zniszczył węzeł albo zerwało się połączenie)
//! kończy program głośno zamiast zostawiać go jako cichego zombie. Rutynowy
//! błąd sesyjny od WirePlumbera — dostarczany jako zdarzenie `Error` BEZ
//! zmiany stanu strumienia — jest tylko logowany, z dławieniem; szczegóły
//! przy `verdict`. To samo dotyczy śmierci dowolnego wątku toru AI —
//! sygnalizuje ją `health_rx` przekazany z `pipeline::spawn`.
//!
//! CZEGO TU NIE MA: cel odtwarzania jest ustalany RAZ, przy starcie
//! (`pick_output`), i NIE podąża za zmianą domyślnego wyjścia w KDE.
//! `lutils.checkFollowDefault` (linking-utils.lua:180-184) wymaga
//! `reconnect and not is_filter`, a my mamy i DONT_RECONNECT, i
//! `node.link-group` (`is_filter = si_props["node.link-group"] ~= nil`).
//! Zmiana wyjścia w aplecie jest więc dla nas wyłącznie LOGOWANA
//! (`DefaultSinkWatch`), a faktyczne przepięcie wymaga restartu programu.

use anyhow::{bail, Result};
use pipewire as pw;
use pw::stream::{StreamBox, StreamFlags, StreamState};
use pw::{properties::properties, spa};
use ringbuf::{traits::*, HeapCons, HeapProd};
use spa::param::audio::{AudioFormat, AudioInfoRaw, MAX_CHANNELS};
use spa::param::format::{MediaSubtype, MediaType};
use spa::param::format_utils;
use spa::pod::Pod;
use spa::utils::Direction;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use std::{cell::Cell, cell::RefCell, rc::Rc};

pub const RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;
const STRIDE: usize = CHANNELS * std::mem::size_of::<f32>();
/// maksymalna liczba ramek przetwarzanych jednym kawałkiem w callbacku RT
const SCRATCH_FRAMES: usize = 4096;
/// gdy graf nie zasugeruje `requested` (adapter, req=0) — bezpieczny cap
const FALLBACK_FRAMES: usize = 1024;
/// jak często raportować w logu przepełnienia ringów RT
const DROP_REPORT_INTERVAL: Duration = Duration::from_secs(5);

/// Pomiar zaległości ringu passthrough — WYŁĄCZNIE diagnostyka, żadna
/// wartość stąd nie wchodzi do decyzji w torze RT.
///
/// Po co: zaległość tego ringu jest dziś niezmiernikiem, którego NIKT nie
/// mierzy. Teza "prime z main.rs zostaje w ringu na zawsze i daje stałe
/// ~85 ms opóźnienia" jest wyprowadzona, nie zmierzona — a równie dobrze
/// prime może wyparować w pierwszych kilku cyklach (sink nie produkuje, gdy
/// nic do niego nie gra, a odtwarzanie drenuje co cykl). Regulator zaległości
/// wolno dodać dopiero, gdy ta liczba jest znana, i z celem dobranym do niej,
/// a nie do liczby z kartki.
///
/// Zapis z callbacku RT: pięć operacji atomowych `Relaxed` na cykl, bez blokad,
/// bez alokacji, bez IO.
///
/// CELOWO bez `#[derive(Default)]`: `min` musi startować z `u64::MAX`, inaczej
/// `fetch_min` nigdy nie zejdzie poniżej zera i struktura na zawsze raportuje
/// „min 0,0 ms" — cicho, wiarygodnie i fałszywie, w jedynej metryce, dla której
/// ta struktura powstała. Jedyny konstruktor to `new()`.
pub struct PassStats {
    /// zaległość (w próbkach f32, stereo interleaved) z ostatniego cyklu
    last: AtomicU64,
    /// minimum od ostatniego raportu (u64::MAX = brak próbek)
    min: AtomicU64,
    /// maksimum od ostatniego raportu
    max: AtomicU64,
    /// największy NIEZEROWY `buf.requested()` w oknie; 0 znaczy „graf ani razu
    /// nie podał kwantu" (adapter, req=0), a nie „kwant wynosi zero"
    quantum: AtomicU64,
    /// liczba cykli od ostatniego raportu
    cycles: AtomicU64,
}

/// Migawka pomiaru zebrana przez wątek raportujący.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct PassSnapshot {
    pub min_samples: u64,
    pub max_samples: u64,
    pub last_samples: u64,
    pub quantum_frames: u64,
    pub cycles: u64,
}

impl PassStats {
    pub fn new() -> Self {
        Self {
            last: AtomicU64::new(0),
            min: AtomicU64::new(u64::MAX),
            max: AtomicU64::new(0),
            quantum: AtomicU64::new(0),
            cycles: AtomicU64::new(0),
        }
    }

    /// [RT] wołane raz na cykl callbacku odtwarzania.
    #[inline]
    fn observe(&self, backlog_samples: u64, quantum_frames: u64) {
        self.last.store(backlog_samples, Ordering::Relaxed);
        self.min.fetch_min(backlog_samples, Ordering::Relaxed);
        self.max.fetch_max(backlog_samples, Ordering::Relaxed);
        // `requested == 0` jest legalne (adapter nie sugeruje kwantu — patrz
        // FALLBACK_FRAMES) i NIE jest zmierzonym kwantem. Przy `store`
        // wystarczyłby JEDEN taki cykl na końcu okna, żeby raport wypisał
        // „kwant 0 ramek = 0.0 ms", a przyszły regulator zaległości (cel
        // liczony z kwantu) dostał zero jako podstawę.
        if quantum_frames > 0 {
            self.quantum.fetch_max(quantum_frames, Ordering::Relaxed);
        }
        self.cycles.fetch_add(1, Ordering::Relaxed);
    }

    /// Odczyt + zerowanie okna min/max. Zwraca `None`, gdy w oknie nie było
    /// ani jednego cyklu (odtwarzanie stoi — to samo w sobie jest informacją,
    /// ale nie ma czego uśredniać).
    pub fn take(&self) -> Option<PassSnapshot> {
        let cycles = self.cycles.swap(0, Ordering::Relaxed);
        let min = self.min.swap(u64::MAX, Ordering::Relaxed);
        let max = self.max.swap(0, Ordering::Relaxed);
        // kwant też należy do okna — inaczej raport pokazywałby wartość
        // odziedziczoną po oknie, w którym graf jeszcze coś podawał
        let quantum = self.quantum.swap(0, Ordering::Relaxed);
        if cycles == 0 || min == u64::MAX {
            return None;
        }
        Some(PassSnapshot {
            min_samples: min,
            max_samples: max,
            last_samples: self.last.load(Ordering::Relaxed),
            quantum_frames: quantum,
            cycles,
        })
    }
}

/// Próbki ringu passthrough (stereo interleaved) → milisekundy odsłuchu.
pub fn pass_samples_to_ms(samples: u64) -> f32 {
    samples as f32 / CHANNELS as f32 / RATE as f32 * 1000.0
}

pub const SINK_NODE_NAME: &str = "nacelle-translator-sink";
pub const OUT_NODE_NAME: &str = "nacelle-translator-out";
/// prefiksy node.name wskazujące realny sprzęt — jedyne dopuszczalne przy
/// automatycznym wyborze celu (nigdy cudzy wirtualny sink: pętla sprzężenia)
const HARDWARE_NAME_PREFIXES: &[&str] = &["alsa_output.", "bluez_output."];

#[derive(Clone, Debug)]
pub struct SinkInfo {
    pub id: u32,
    pub name: String,
    pub description: String,
}

/// Ringbuffery łączące callbacki RT z resztą programu.
pub struct RtRings {
    /// mono 48 kHz z wirtualnego sinka → segmenter
    pub cap_prod: HeapProd<f32>,
    /// stereo interleaved 48 kHz: oryginał (passthrough)
    pub pass_prod: HeapProd<f32>,
    pub pass_cons: HeapCons<f32>,
    /// mono 48 kHz: głos lektora z TTS
    pub tts_cons: HeapCons<f32>,
}

/// Bramka toru AI — JEDYNE miejsce, które decyduje, czy dźwięk przechodzący
/// przez węzeł jest w ogóle mielony przez AI.
///
/// Po co osobny typ zamiast gołego `bool`: bramka steruje DWOMA callbackami
/// RT w różnych strumieniach (karmienie `cap_prod` w sinku, odtwarzanie
/// lektora w playbacku) i te dwie decyzje MUSZĄ być zgodne. Rozjazd nie
/// wywala programu, tylko daje ciche patologie: bramka „karm AI, nie graj
/// lektora" pali GPU bez efektu, a odwrotna („graj lektora, nie karm AI")
/// zostawia w ringu resztkę mowy z poprzedniej sesji i ściszy oryginał pod
/// zdanie, które nie ma już do czego pasować. Jeden typ z dwoma metodami
/// czytającymi to samo pole zamyka tę klasę błędów, a test `b3` pilnuje
/// niezmiennika.
///
/// KOSZT W RT: pole jest kopiowane do stanu callbacku PRZY STARCIE i dalej
/// czytane jako zwykły `bool` — bez atomiku, bez blokady, bez alokacji.
/// Wartość zapada raz, przy wczytaniu konfiguracji; nie ma przełącznika
/// w locie, więc atomik byłby tylko droższym zapisem tego samego faktu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TranslateGate {
    on: bool,
}

impl TranslateGate {
    pub fn new(on: bool) -> Self {
        Self { on }
    }

    /// [RT] czy wpychać mono do `cap_prod` (wejście segmentera/VAD).
    /// `false` = tor AI nie dostaje ani jednej próbki, więc whisper nigdy
    /// nie rusza i GPU stoi.
    #[inline]
    pub fn feeds_ai(self) -> bool {
        self.on
    }

    /// [RT] czy w ogóle drenować ring TTS i mieszać lektora (a więc i
    /// duckować oryginał).
    #[inline]
    pub fn plays_tts(self) -> bool {
        self.on
    }

    /// Jedna linia do logu startowego — żeby po fakcie dało się z logu sesji
    /// odczytać, w którym trybie program chodził.
    pub fn describe(self) -> &'static str {
        if self.on {
            "tłumaczenie WŁĄCZONE: dźwięk przechodzący przez węzeł idzie do VAD/whisper \
             i może zostać przeczytany przez lektora (oryginał jest wtedy przyciszany)"
        } else {
            "tłumaczenie WYŁĄCZONE ([audio].translate = false): węzeł jest czystą przelotką, \
             tor AI nie dostaje ani jednej próbki, GPU stoi — włącz kluczem \
             [audio].translate = true w nacelle-translator.toml"
        }
    }
}

/// Parametry duckingu przeliczone na współczynniki per próbka @48 kHz.
#[derive(Clone, Copy)]
pub struct DuckParams {
    pub pass_gain: f32,
    pub duck_gain: f32,
    pub tts_gain: f32,
    pub attack_coef: f32,
    pub release_coef: f32,
    pub hold_frames: u32,
}

impl DuckParams {
    pub fn from_cfg(a: &crate::config::AudioCfg) -> Self {
        let coef = |ms: f32| 1.0 - (-1.0 / (ms.max(1.0) / 1000.0 * RATE as f32)).exp();
        Self {
            pass_gain: a.passthrough_gain,
            duck_gain: a.duck_gain,
            tts_gain: a.tts_gain,
            attack_coef: coef(a.duck_attack_ms),
            release_coef: coef(a.duck_release_ms),
            hold_frames: (a.duck_hold_ms / 1000.0 * RATE as f32) as u32,
        }
    }
}

/// Miękki limiter: identyczność poniżej progu, asymptotyczne podejście do
/// ±1.0 powyżej — zamiast twardego obcięcia (flat-top, słyszalny trzask),
/// które przy głośnym oryginale + starcie lektora potrafi wystąpić na
/// każdej wypowiedzi (suma pl*gain + t chwilowo > 1.0 w oknie ataku).
#[inline]
fn soft_clip(x: f32) -> f32 {
    const T: f32 = 0.9;
    if x.abs() <= T {
        x
    } else {
        let sign = x.signum();
        sign * (T + (1.0 - T) * ((x.abs() - T) / (1.0 - T)).tanh())
    }
}

/// Pod EnumFormat: F32 interleaved, 48 kHz, stereo FL/FR.
fn format_pod_bytes() -> Vec<u8> {
    let mut info = AudioInfoRaw::new();
    info.set_format(AudioFormat::F32LE); // interleaved (planarny byłby F32P)
    info.set_rate(RATE);
    info.set_channels(CHANNELS as u32);
    let mut pos = [0u32; MAX_CHANNELS];
    pos[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
    pos[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
    info.set_position(pos);

    spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(spa::pod::Object {
            type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
            id: spa::param::ParamType::EnumFormat.as_raw(),
            properties: info.into(),
        }),
    )
    .unwrap()
    .0
    .into_inner()
}

/// Pełna lista globali dopiero po roundtripie (core.sync + done). Obsługuje
/// też błąd połączenia z demonem (core.error) — bez tego zerwanie socketu
/// między sync a done zawiesza mainloop.run() na zawsze.
fn enumerate_sinks(
    mainloop: &pw::main_loop::MainLoopRc,
    core: &pw::core::CoreRc,
) -> Result<Vec<SinkInfo>> {
    let registry = core.get_registry_rc()?;
    let sinks = Rc::new(RefCell::new(Vec::<SinkInfo>::new()));
    let core_err: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));

    let _reg_listener = registry
        .add_listener_local()
        .global({
            let sinks = sinks.clone();
            move |g| {
                if g.type_ != pw::types::ObjectType::Node {
                    return;
                }
                let Some(props) = g.props else { return };
                if props.get("media.class") != Some("Audio/Sink") {
                    return;
                }
                sinks.borrow_mut().push(SinkInfo {
                    id: g.id,
                    name: props.get("node.name").unwrap_or_default().to_string(),
                    description: props.get("node.description").unwrap_or_default().to_string(),
                });
            }
        })
        .register();

    let pending = core.sync(0)?;
    let _core_listener = core
        .add_listener_local()
        .done({
            let ml = mainloop.clone();
            move |id, seq| {
                if id == pw::core::PW_ID_CORE && seq == pending {
                    ml.quit();
                }
            }
        })
        .error({
            let ml = mainloop.clone();
            let core_err = core_err.clone();
            move |id, _seq, res, msg| {
                if id == pw::core::PW_ID_CORE {
                    *core_err.borrow_mut() = Some(format!("{msg} (res={res})"));
                    ml.quit();
                }
            }
        })
        .register();
    mainloop.run();

    if let Some(msg) = core_err.take() {
        bail!("połączenie z PipeWire przerwane podczas enumeracji węzłów: {msg}");
    }
    Ok(sinks.take())
}

/// Odczytuje (WYŁĄCZNIE do odczytu — żadnego zapisu do metadanych, żadnego
/// przełączania systemowego domyślnego wyjścia) aktualnie skonfigurowane
/// domyślne wyjście audio z obiektu metadanych PipeWire "default". To
/// dokładnie to, co pokazuje `wpctl status` pod "Default Configured
/// Devices" i co ustawia się w KDE — czyli "urządzenie, które mam ustawione
/// jako obecne urządzenie wyjścia dźwięku".
///
/// Dwa przebiegi sync/done: pierwszy enumeruje rejestr i po drodze wiąże
/// (bind) obiekt metadanych "default"; drugi gwarantuje, że początkowy
/// zrzut jej właściwości (wysyłany przez serwer zaraz po bind) już dotarł —
/// bez tego jest realne ryzyko wyścigu (pierwszy `done` mógłby przyjść
/// zanim serwer w ogóle przetworzy nasz `bind`).
///
/// Zwraca też `DefaultSinkWatch` — uchwyty trzymające podpięcie przy życiu.
/// Dopóki żyją, `property()` woła się przy KAŻDEJ późniejszej zmianie
/// domyślnego wyjścia (kliknięcie w aplecie KDE) bez odpytywania.
fn read_default_sink_name(
    mainloop: &pw::main_loop::MainLoopRc,
    core: &pw::core::CoreRc,
) -> (Option<String>, DefaultSinkWatch) {
    let our_target: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let registry = match core.get_registry_rc() {
        Ok(r) => r,
        Err(e) => {
            log::debug!("nie mogę pobrać rejestru do odczytu domyślnego wyjścia: {e:#}");
            return (None, DefaultSinkWatch::inert(our_target));
        }
    };
    let metadata_bound: Rc<RefCell<Option<pw::metadata::Metadata>>> = Rc::new(RefCell::new(None));
    let metadata_listener: Rc<RefCell<Option<pw::metadata::MetadataListener>>> =
        Rc::new(RefCell::new(None));
    let configured: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let active: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));

    let _reg_listener = registry
        .add_listener_local()
        .global({
            let registry = registry.clone();
            let metadata_bound = metadata_bound.clone();
            let metadata_listener = metadata_listener.clone();
            let configured = configured.clone();
            let active = active.clone();
            let our_target = our_target.clone();
            move |g| {
                if g.type_ != pw::types::ObjectType::Metadata
                    || metadata_bound.borrow().is_some()
                {
                    return;
                }
                if g.props.and_then(|p| p.get("metadata.name")) != Some("default") {
                    return;
                }
                let Ok(md) = registry.bind::<pw::metadata::Metadata, _>(g) else {
                    return;
                };
                let listener = md
                    .add_listener_local()
                    .property({
                        let configured = configured.clone();
                        let active = active.clone();
                        let our_target = our_target.clone();
                        move |_subject, key, _type_, value| {
                            let slot = match key {
                                Some("default.configured.audio.sink") => Some(&configured),
                                Some("default.audio.sink") => Some(&active),
                                _ => None,
                            };
                            if let (Some(slot), Some(v)) = (slot, value) {
                                if let Ok(j) = serde_json::from_str::<serde_json::Value>(v) {
                                    if let Some(n) = j["name"].as_str() {
                                        *slot.borrow_mut() = Some(n.to_string());
                                        // `our_target` jest wypełniane dopiero po
                                        // `pick_output`, więc przy starcie ta gałąź
                                        // milczy — odzywa się WYŁĄCZNIE przy
                                        // późniejszej zmianie zrobionej przez
                                        // użytkownika.
                                        if key == Some("default.configured.audio.sink") {
                                            if let Some(t) = our_target.borrow().as_deref() {
                                                if n != t && n != SINK_NODE_NAME {
                                                    log::warn!(
                                                        "domyślne wyjście dźwięku zmieniono na \
                                                         \"{n}\", ale translator gra dalej w \
                                                         \"{t}\" — cel jest ustalany raz, przy \
                                                         starcie, i nie podąża za apletem \
                                                         (DONT_RECONNECT + node.link-group \
                                                         wyłączają checkFollowDefault \
                                                         WirePlumbera). Żeby przepiąć: \
                                                         zrestartuj translator (albo wskaż cel \
                                                         w [audio].output_device)."
                                                    );
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            0
                        }
                    })
                    .register();
                *metadata_bound.borrow_mut() = Some(md);
                *metadata_listener.borrow_mut() = Some(listener);
            }
        })
        .register();

    // przebieg 1: enumeracja + bind (request bind idzie do serwera w trakcie tego run())
    if let Err(e) = (|| -> Result<()> {
        let pending = core.sync(0)?;
        let _l = core
            .add_listener_local()
            .done({
                let ml = mainloop.clone();
                move |id, seq| {
                    if id == pw::core::PW_ID_CORE && seq == pending {
                        ml.quit();
                    }
                }
            })
            .register();
        mainloop.run();
        Ok(())
    })() {
        log::debug!("odczyt domyślnego wyjścia: {e:#}");
        return (None, DefaultSinkWatch::inert(our_target));
    }

    // przebieg 2: gwarantuje dotarcie początkowego zrzutu property() z metadanych
    if let Err(e) = (|| -> Result<()> {
        let pending = core.sync(0)?;
        let _l = core
            .add_listener_local()
            .done({
                let ml = mainloop.clone();
                move |id, seq| {
                    if id == pw::core::PW_ID_CORE && seq == pending {
                        ml.quit();
                    }
                }
            })
            .register();
        mainloop.run();
        Ok(())
    })() {
        log::debug!("odczyt domyślnego wyjścia (przebieg 2): {e:#}");
        return (None, DefaultSinkWatch::inert(our_target));
    }

    // `borrow().clone()` zamiast `take()`: podpięcie ma żyć dalej i porównywać
    // przyszłe zmiany, więc nie wolno opróżnić slotów.
    let name = configured
        .borrow()
        .clone()
        .or_else(|| active.borrow().clone());
    let watch = DefaultSinkWatch {
        our_target,
        _md: metadata_bound.take(),
        _md_listener: metadata_listener.take(),
    };
    (name, watch)
}

/// Żywe podpięcie do obiektu metadanych "default" — WYŁĄCZNIE do odczytu.
///
/// Dopóki uchwyty żyją, serwer woła nasz `property()` przy każdej zmianie
/// domyślnego wyjścia. Program niczego tu nie zapisuje ani nie przełącza;
/// jedyny efekt zmiany to ostrzeżenie w logu, że translator gra dalej
/// w urządzenie wybrane przy starcie (patrz nagłówek pliku, „CZEGO TU NIE
/// MA"). Bez tego podpięcia rozjazd „wybrałem głośniki, a słychać słuchawki"
/// jest całkowicie niewidoczny.
struct DefaultSinkWatch {
    /// nazwa węzła, w który faktycznie gramy; wypełniana po `pick_output`
    our_target: Rc<RefCell<Option<String>>>,
    _md: Option<pw::metadata::Metadata>,
    _md_listener: Option<pw::metadata::MetadataListener>,
}

impl DefaultSinkWatch {
    /// wariant bez podpięcia (nie udało się dobić do metadanych) — wszystkie
    /// metody działają, tylko nikt nigdy nie zawoła `property()`
    fn inert(our_target: Rc<RefCell<Option<String>>>) -> Self {
        Self {
            our_target,
            _md: None,
            _md_listener: None,
        }
    }

    fn set_target(&self, name: &str) {
        *self.our_target.borrow_mut() = Some(name.to_string());
    }
}

/// Enumeracja węzłów Audio/Sink + odczyt aktualnie skonfigurowanego
/// domyślnego wyjścia, w jednej sesji (podkomendy `devices` i `check`).
pub fn discover_sinks() -> Result<(Vec<SinkInfo>, Option<String>)> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;
    let sinks = enumerate_sinks(&mainloop, &core)?;
    let (default, _watch) = read_default_sink_name(&mainloop, &core);
    Ok((sinks, default))
}

/// Wybór sprzętowego celu odtwarzania — w kolejności:
///  1. `output_device` z konfiguracji (jawny wybór użytkownika),
///  2. aktualnie skonfigurowane domyślne wyjście odczytane z PipeWire
///     (dokładnie "urządzenie, które mam ustawione jako obecne urządzenie
///     wyjścia dźwięku"; TYLKO odczyt — program niczego tu nie przełącza),
///  3. pierwszy węzeł sprzętowy pasujący do prefiksu (ostatnia deska
///     ratunku, gdy powyższe zawiodą).
/// Nigdy własny węzeł (pętla!) i nigdy cudzy wirtualny sink (np.
/// EasyEffects) — jego wyjście gra do domyślnego urządzenia, którym może
/// być właśnie nasz sink, co zapętla graf.
pub fn pick_output(
    sinks: &[SinkInfo],
    requested: Option<&str>,
    default_sink: Option<&str>,
) -> Result<SinkInfo> {
    let own = [SINK_NODE_NAME, OUT_NODE_NAME];
    // pusty string w konfigu (np. wyczyszczone pole zamiast zakomentowane)
    // ma znaczyć to samo co brak wartości — automatyczny wybór
    let requested = requested.filter(|s| !s.is_empty());
    if let Some(name) = requested {
        if own.contains(&name) {
            bail!("output_device wskazuje na własny węzeł translatora — to byłaby pętla");
        }
        if let Some(s) = sinks.iter().find(|s| s.name == name) {
            return Ok(s.clone());
        }
        bail!(
            "nie znalazłem urządzenia \"{name}\" wśród węzłów Audio/Sink \
             (lista: nacelle-translator devices)"
        );
    }
    // Gdy metadana wskazuje na nas samych (użytkownik wcześniej wybrał
    // "Nacelle Translator (PL)" jako domyślne wyjście), informacja o prawdziwym
    // sprzęcie jest bezpowrotnie nadpisana — spadamy do heurystyki niżej.
    if let Some(name) = default_sink.filter(|n| !own.contains(n)) {
        if let Some(s) = sinks.iter().find(|s| s.name == name) {
            return Ok(s.clone());
        }
    }
    // Ostatnia deska ratunku — działa TYLKO dopóki użytkownik nie wybierze
    // nas jako domyślnego wyjścia (od tego momentu metadana wyżej zawsze
    // wskazuje na nas samych i to jest jedyna ścieżka, jaka zostaje).
    // Zestawy słuchawkowe niemal zawsze wpinają się przez USB albo
    // Bluetooth, nie przez wbudowane audio płyty głównej (PCI) — wolimy je,
    // żeby zgadywanka nie trafiała regularnie w onboard zamiast słuchawek.
    sinks
        .iter()
        .filter(|s| {
            !own.contains(&s.name.as_str())
                && HARDWARE_NAME_PREFIXES
                    .iter()
                    .any(|p| s.name.starts_with(p))
        })
        .max_by_key(|s| {
            if s.name.contains("usb-") {
                2
            } else if s.name.starts_with("bluez_output.") {
                1
            } else {
                0
            }
        })
        .cloned()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "brak jednoznacznego sprzętowego węzła Audio/Sink (alsa_output.*/bluez_output.*) — \
                 ustaw [audio].output_device w nacelle-translator.toml (lista: nacelle-translator devices)"
            )
        })
}

struct SinkState {
    cap_prod: HeapProd<f32>,
    pass_prod: HeapProd<f32>,
    stereo: Vec<f32>,
    mono: Vec<f32>,
    cap_dropped: Arc<AtomicU64>,
    pass_dropped: Arc<AtomicU64>,
    /// kopia bramki — czytana w callbacku RT jako zwykły `bool`
    gate: TranslateGate,
}

struct PlayState {
    pass_cons: HeapCons<f32>,
    tts_cons: HeapCons<f32>,
    pass_scratch: Vec<f32>,
    tts_scratch: Vec<f32>,
    duck: DuckParams,
    gain: f32,
    hold: u32,
    /// tylko pomiar (Etap 1a) — nie wpływa na ani jedną próbkę
    stats: Arc<PassStats>,
    /// kopia bramki — czytana w callbacku RT jako zwykły `bool`
    gate: TranslateGate,
}

/// Współdzielony stan fatalny: ustawiany z callbacków `state_changed`/`error`
/// (main-loop, nie RT) i ze strażnika wątków toru AI; po `mainloop.run()`
/// zamieniany na `Err`, żeby proces zakończył się głośno zamiast wisieć.
type Fatal = Rc<RefCell<Option<String>>>;

/// Co strażnik ma zrobić z przejściem stanu.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Verdict {
    Ignore,
    Warn,
    Fatal,
}

/// `StreamState::Error` przychodzi z DWÓCH źródeł i znaczą one co innego:
///
///  1. `proxy_error` (PipeWire 1.6.8, stream.c:1241-1252) emituje
///     `state_changed(stream, stream->state, PW_STREAM_STATE_ERROR, message)`
///     BEZ ZMIANY `stream->state` — sygnał czysto informacyjny. Tą drogą
///     przychodzą rutynowe błędy sesyjne od WirePlumbera, rozsyłane przez
///     `lutils.sendClientError` (linking/prepare-link.lua, find-defined-target.lua,
///     find-filter-target.lua, link-target.lua) — typowo „no target node
///     available". Zabijanie procesu na taki komunikat to reagowanie na
///     zwykłą pracę serwera jak na awarię.
///  2. `pw_stream_set_error` — prawdziwa awaria (odrzucona negocjacja formatu,
///     brak buforów, błąd core). Ta ścieżka stan ZMIENIA.
///
/// Rozróżnia je odczyt `stream.state()` w chwili callbacku: w przypadku 1 jest
/// tam nadal stary stan, w przypadku 2 jest Error. Bez tego rozróżnienia
/// zostajemy albo z procesem umierającym na komunikat informacyjny, albo
/// z niemym zombie po prawdziwym błędzie — a jedno i drugie jest gorsze niż
/// dzisiejsze zachowanie.
///
/// `Unconnected` jest fatalne BEZWARUNKOWO. Wcześniejsza bramka „dopiero po
/// pierwszym Streaming/Paused" była nie tylko zbędna, ale szkodliwa:
///  * `stream_set_state` emituje `state_changed` wyłącznie przy FAKTYCZNEJ
///    zmianie, a stanem początkowym jest `Unconnected` ustawiane bez emisji
///    w `pw_stream_new` — zdarzenie `new == Unconnected` z definicji znaczy
///    więc, że już raz z tego stanu wyszliśmy;
///  * przy PIERWSZEJ nieudanej próbie linkowania WirePlumber robi
///    `sendClientError` (zdarzenie Error → Warn) i NATYCHMIAST
///    `node:request_destroy()` (→ `proxy_removed` → Unconnected). Z bramką
///    o przeżyciu procesu decydował wyścig między klienckim przejściem
///    CONNECTING→PAUSED a zniszczeniem węzła przez serwer. Fatalność
///    strażnika nie może zależeć od wyścigu.
///
/// Uwaga na kolejność zamykania w `run_graph`: nasze własne `disconnect()`
/// też daje `Unconnected`, dlatego `fatal` jest ODCZYTYWANE PRZED
/// rozłączeniem strumieni — inaczej każdy czysty Ctrl+C kończyłby się
/// fałszywym alarmem i uczył ignorować dokładnie ten komunikat.
fn verdict(new: &StreamState, stream_state_is_error: bool) -> Verdict {
    match new {
        StreamState::Error(_) if stream_state_is_error => Verdict::Fatal,
        StreamState::Error(_) => Verdict::Warn,
        StreamState::Unconnected => Verdict::Fatal,
        _ => Verdict::Ignore,
    }
}

/// Jak często meldować rutynowy błąd sesyjny. Warunek („brak celu", „nieudany
/// link") jest z natury trwały, a WirePlumber wysyła `sendClientError` przy
/// KAŻDYM rescanie — bez dławienia zalewamy poziom WARN, na którym widać
/// jedyny czysty sygnał alarmowy toru RT (przepełnienia ringów).
const SESSION_WARN_INTERVAL: Duration = Duration::from_secs(10);

fn watch_stream<D>(
    name: &'static str,
    target_name: &str,
    fatal: &Fatal,
    ml: &pw::main_loop::MainLoopRc,
) -> impl FnMut(&pw::stream::Stream, &mut D, StreamState, StreamState) {
    let fatal = fatal.clone();
    let ml = ml.clone();
    let target_name = target_name.to_string();
    let last_warn: Cell<Option<std::time::Instant>> = Cell::new(None);
    let suppressed = Cell::new(0u64);
    // ostatni komunikat sesyjny — jedyna konkretna wskazówka, jaką serwer nam
    // dał, i akurat ta, której brakowało w komunikacie fatalnym
    let last_session_error = RefCell::new(String::new());
    move |stream, _ud, old, new| {
        log::debug!("{name}: stan {old:?} -> {new:?}");
        let state_is_error = matches!(stream.state(), StreamState::Error(_));
        match verdict(&new, state_is_error) {
            Verdict::Ignore => {}
            Verdict::Warn => {
                let msg = match &new {
                    StreamState::Error(m) => m.as_str(),
                    _ => "",
                };
                *last_session_error.borrow_mut() = msg.to_string();
                let now = std::time::Instant::now();
                let due = match last_warn.get() {
                    None => true,
                    Some(t) => now.duration_since(t) >= SESSION_WARN_INTERVAL,
                };
                if due {
                    let skipped = suppressed.replace(0);
                    let tail = if skipped > 0 {
                        format!(" (pominięto {skipped} podobnych w ostatnich {}s)",
                            SESSION_WARN_INTERVAL.as_secs())
                    } else {
                        String::new()
                    };
                    log::warn!(
                        "{name}: błąd sesyjny od serwera przy celu \"{target_name}\" \
                         (zwykle brak celu albo nieudany link), stan strumienia bez zmian — \
                         pracuję dalej: {msg}{tail}"
                    );
                    last_warn.set(Some(now));
                } else {
                    suppressed.set(suppressed.get() + 1);
                }
            }
            Verdict::Fatal => {
                let last = last_session_error.borrow();
                let hint = if last.is_empty() {
                    String::new()
                } else {
                    format!(" Ostatni komunikat serwera: {last}.")
                };
                let reason = match &new {
                    StreamState::Error(m) => {
                        format!("{name}: błąd strumienia (cel \"{target_name}\"): {m}")
                    }
                    _ => format!(
                        "{name}: węzeł wypadł z grafu — cel odtwarzania \"{target_name}\" \
                         zniknął albo zerwało się połączenie z PipeWire.{hint} \
                         Co zrobić: włącz to urządzenie i uruchom translator ponownie, \
                         albo wskaż inne w [audio].output_device \
                         (lista: nacelle-translator devices)."
                    ),
                };
                drop(last);
                *fatal.borrow_mut() = Some(reason);
                ml.quit();
            }
        }
    }
}

/// Buduje graf i blokuje w pętli głównej do SIGINT/SIGTERM, błędu strumienia
/// albo śmierci dowolnego wątku toru AI (`health_rx`).
pub fn run_graph(
    output_device: Option<&str>,
    rings: RtRings,
    duck: DuckParams,
    gate: TranslateGate,
    health_rx: crossbeam_channel::Receiver<String>,
) -> Result<()> {
    pw::init();
    // Jedna linia, żeby z logu sesji dało się odczytać tryb pracy bez
    // zgadywania „czy on w ogóle tłumaczył".
    log::info!("{}", gate.describe());

    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;

    let sinks = enumerate_sinks(&mainloop, &core)?;
    let (default_sink, default_watch) = read_default_sink_name(&mainloop, &core);
    let target = pick_output(&sinks, output_device, default_sink.as_deref())?;
    // Od tej chwili podpięcie do metadanych ma co porównywać. `default_watch`
    // MUSI dożyć końca `run_graph` — jego wcześniejszy drop wypina słuchacza
    // i zmiana wyjścia w KDE znów staje się niewidoczna.
    default_watch.set_target(&target.name);
    log::info!(
        "cel odtwarzania: {} ({})",
        target.name,
        target.description
    );

    let RtRings {
        cap_prod,
        pass_prod,
        pass_cons,
        tts_cons,
    } = rings;

    let fatal: Fatal = Rc::new(RefCell::new(None));
    let cap_dropped = Arc::new(AtomicU64::new(0));
    let pass_dropped = Arc::new(AtomicU64::new(0));
    let pass_stats = Arc::new(PassStats::new());

    // ---------- 1) wirtualny sink (Direction::Input) ----------
    let sink_stream = StreamBox::new(
        &core,
        SINK_NODE_NAME,
        properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            "media.class" => "Audio/Sink",
            *pw::keys::NODE_NAME => SINK_NODE_NAME,
            *pw::keys::NODE_DESCRIPTION => "Nacelle Translator (PL)",
            "node.virtual" => "true",
            "node.link-group" => "nacelle-translator",
            "audio.position" => "[ FL FR ]",
        },
    )?;

    let sink_state = SinkState {
        cap_prod,
        pass_prod,
        stereo: vec![0.0; SCRATCH_FRAMES * CHANNELS],
        mono: vec![0.0; SCRATCH_FRAMES],
        cap_dropped: cap_dropped.clone(),
        pass_dropped: pass_dropped.clone(),
        gate,
    };

    let _sink_listener = sink_stream
        .add_local_listener_with_user_data(sink_state)
        .state_changed(watch_stream("sink", &target.name, &fatal, &mainloop))
        .param_changed(|_stream, _st, id, param| {
            let Some(param) = param else { return };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            let Ok((mt, mst)) = format_utils::parse_format(param) else {
                return;
            };
            if mt != MediaType::Audio || mst != MediaSubtype::Raw {
                return;
            }
            let mut info = AudioInfoRaw::default();
            if info.parse(param).is_ok() {
                log::info!(
                    "sink: wynegocjowany format {} Hz, {} kan.",
                    info.rate(),
                    info.channels()
                );
            }
        })
        .process(|stream, st: &mut SinkState| {
            // Wątek RT: zero alokacji, blokad i IO.
            let Some(mut buf) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buf.datas_mut();
            let Some(d) = datas.first_mut() else { return };
            let (offs, size) = {
                let c = d.chunk();
                (c.offset() as usize, c.size() as usize)
            };
            let Some(bytes) = d.data() else { return };
            let end = (offs + size).min(bytes.len());
            if offs >= end {
                return;
            }
            let payload = &bytes[offs..end];

            let mut pos = 0usize;
            while pos < payload.len() {
                let nbytes = (payload.len() - pos).min(st.stereo.len() * 4);
                let nbytes = nbytes - nbytes % STRIDE;
                if nbytes == 0 {
                    break;
                }
                let nsamples = nbytes / 4;
                for (i, b) in payload[pos..pos + nbytes].chunks_exact(4).enumerate() {
                    st.stereo[i] = f32::from_le_bytes([b[0], b[1], b[2], b[3]]);
                }
                // oryginał w pełnym stereo do toru passthrough
                let pushed = st.pass_prod.push_slice(&st.stereo[..nsamples]);
                if pushed < nsamples {
                    st.pass_dropped
                        .fetch_add((nsamples - pushed) as u64, Ordering::Relaxed);
                }
                // downmix mono dla toru AI; pełny ring => nadmiar przepada.
                // Przy zamkniętej bramce pomijamy CAŁOŚĆ (także sam downmix):
                // przelotka ma wtedy kosztować tyle co przepisanie próbek,
                // a nie tyle co przepisanie plus pętla mnożeń na darmo.
                if st.gate.feeds_ai() {
                    let frames = nsamples / CHANNELS;
                    for f in 0..frames {
                        st.mono[f] = (st.stereo[f * 2] + st.stereo[f * 2 + 1]) * 0.5;
                    }
                    let pushed = st.cap_prod.push_slice(&st.mono[..frames]);
                    if pushed < frames {
                        st.cap_dropped
                            .fetch_add((frames - pushed) as u64, Ordering::Relaxed);
                    }
                }
                pos += nbytes;
            }
        })
        .register()?;

    let sink_fmt = format_pod_bytes(); // musi żyć do connect()
    let mut sink_params = [Pod::from_bytes(&sink_fmt).unwrap()];
    sink_stream.connect(
        Direction::Input,
        None, // PW_ID_ANY
        StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
        &mut sink_params,
    )?;

    // ---------- 2) odtwarzanie (Direction::Output) w sprzęt ----------
    let play_props = {
        let mut p = properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Playback",
            *pw::keys::NODE_NAME => OUT_NODE_NAME,
            "node.link-group" => "nacelle-translator",
            // Klucz WirePlumbera (PipeWire go nie zna — `strings
            // libpipewire-0.3.so.0 | grep dont-` zwraca wyłącznie
            // `node.dont-reconnect`). Czytany wprost z właściwości węzła
            // w linking/find-defined-target.lua:38 i decyduje o gałęzi :116-127.
            // Bez niego `target.object` jest tylko SUGESTIĄ: nietrafione
            // dopasowanie przechodzi dalej, aż do find-default-target.lua,
            // czyli do domyślnego sinka — a tym bywa NASZ sink. Z nim serwer
            // zamiast fallbacku wysyła błąd i niszczy nasz węzeł, co strażnik
            // `verdict` zamienia w głośne zakończenie z instrukcją.
            "node.dont-fallback" => "true",
        };
        p.insert(*pw::keys::TARGET_OBJECT, target.name.as_str());
        p
    };
    let play_stream = StreamBox::new(&core, OUT_NODE_NAME, play_props)?;

    let play_state = PlayState {
        pass_cons,
        tts_cons,
        pass_scratch: vec![0.0; SCRATCH_FRAMES * CHANNELS],
        tts_scratch: vec![0.0; SCRATCH_FRAMES],
        duck,
        gain: duck.pass_gain,
        hold: 0,
        stats: pass_stats.clone(),
        gate,
    };

    let _play_listener = play_stream
        .add_local_listener_with_user_data(play_state)
        .state_changed(watch_stream("playback", &target.name, &fatal, &mainloop))
        .process(|stream, st: &mut PlayState| {
            let Some(mut buf) = stream.dequeue_buffer() else {
                return;
            };
            // Ile ramek graf faktycznie chce w tym cyklu (pole `requested`,
            // dostępne dzięki feature "v1_2_0" >= "v0_3_49"). Bez tego
            // wypełnialibyśmy cały zmapowany bufor (rozmiar quantum-limit,
            // zwykle znacznie większy niż bieżący kwant) i winieślibyśmy
            // sekundy zbędnej latencji oraz wybuchowy, niekontrolowany
            // drenaż ringów pass/tts.
            let requested = buf.requested() as usize;
            // [RT] pomiar zaległości PRZED drenażem ringu — po pętli niżej
            // liczba traci sens. Trzy zapisy atomowe Relaxed, zero blokad.
            st.stats
                .observe(st.pass_cons.occupied_len() as u64, requested as u64);
            let datas = buf.datas_mut();
            let Some(d) = datas.first_mut() else { return };
            let mut total_frames = 0usize;
            if let Some(bytes) = d.data() {
                let max_frames = bytes.len() / STRIDE;
                let want = if requested > 0 {
                    requested.min(max_frames)
                } else {
                    max_frames.min(FALLBACK_FRAMES)
                };
                let usable = want * STRIDE;
                let out: &mut [f32] = bytemuck::cast_slice_mut(&mut bytes[..usable]);
                let frames = out.len() / CHANNELS;
                total_frames = frames;

                let mut fi = 0usize;
                while fi < frames {
                    let n = (frames - fi).min(SCRATCH_FRAMES);
                    let got_pass = st.pass_cons.pop_slice(&mut st.pass_scratch[..n * CHANNELS]);
                    let got_pass_frames = got_pass / CHANNELS;
                    // Zamknięta bramka: ring TTS ZOSTAJE NIETKNIĘTY. Nie
                    // drenujemy go „na wszelki wypadek" — przy zamkniętej
                    // bramce tor AI nie dostaje próbek, więc nikt do tego
                    // ringu nie pisze, a `got_tts = 0` gwarantuje dodatkowo,
                    // że ducking ani razu się nie odpali.
                    let got_tts = if st.gate.plays_tts() {
                        st.tts_cons.pop_slice(&mut st.tts_scratch[..n])
                    } else {
                        0
                    };

                    for j in 0..n {
                        let has_tts = j < got_tts;
                        // Duck zawsze, gdy TTS gra TERAZ; poza tym trzymaj
                        // przyciszenie jeszcze `hold_frames` po jego końcu.
                        // (Warunek musi sprawdzać `has_tts` wprost — samo
                        // "hold > 0" ustawione w tej samej iteracji, w
                        // której hold właśnie przypisano, prowadziłoby przy
                        // hold_frames == 0 do duckingu, który nigdy się nie
                        // uruchamia.)
                        let target_gain = if has_tts {
                            st.hold = st.duck.hold_frames;
                            st.duck.duck_gain
                        } else if st.hold > 0 {
                            st.hold -= 1;
                            st.duck.duck_gain
                        } else {
                            st.duck.pass_gain
                        };
                        let coef = if target_gain < st.gain {
                            st.duck.attack_coef
                        } else {
                            st.duck.release_coef
                        };
                        st.gain += (target_gain - st.gain) * coef;

                        let (pl, pr) = if j < got_pass_frames {
                            (st.pass_scratch[j * 2], st.pass_scratch[j * 2 + 1])
                        } else {
                            (0.0, 0.0) // underrun passthrough => cisza
                        };
                        let t = if has_tts {
                            st.tts_scratch[j] * st.duck.tts_gain
                        } else {
                            0.0
                        };
                        out[(fi + j) * 2] = soft_clip(pl * st.gain + t);
                        out[(fi + j) * 2 + 1] = soft_clip(pr * st.gain + t);
                    }
                    fi += n;
                }
            }
            let chunk = d.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = STRIDE as i32;
            *chunk.size_mut() = (total_frames * STRIDE) as u32;
        })
        .register()?;

    let play_fmt = format_pod_bytes();
    let mut play_params = [Pod::from_bytes(&play_fmt).unwrap()];
    play_stream.connect(
        Direction::Output,
        None,
        StreamFlags::AUTOCONNECT
            | StreamFlags::MAP_BUFFERS
            | StreamFlags::RT_PROCESS
            | StreamFlags::DONT_RECONNECT,
        &mut play_params,
    )?;

    // ---------- czyste zamknięcie: SIGINT/SIGTERM ----------
    let ml = mainloop.clone();
    let _sigint = mainloop
        .loop_()
        .add_signal_local(pw::loop_::Signal::INT, move || ml.quit());
    let ml2 = mainloop.clone();
    let _sigterm = mainloop
        .loop_()
        .add_signal_local(pw::loop_::Signal::TERM, move || ml2.quit());

    // ---------- śmierć dowolnego wątku toru AI => głośne zamknięcie ----------
    let (fatal_tx, fatal_rx) = pw::channel::channel::<String>();
    let _health_attached = fatal_rx.attach(mainloop.loop_(), {
        let fatal = fatal.clone();
        let ml = mainloop.clone();
        move |reason: String| {
            *fatal.borrow_mut() = Some(format!("wątek toru AI zakończył pracę: {reason}"));
            ml.quit();
        }
    });
    std::thread::Builder::new()
        .name("health-watchdog".into())
        .spawn(move || {
            if let Ok(reason) = health_rx.recv() {
                let _ = fatal_tx.send(reason);
            }
        })?;

    // ---------- okresowy raport przepełnień + pomiar zaległości ----------
    {
        let cap_dropped = cap_dropped.clone();
        let pass_dropped = pass_dropped.clone();
        let pass_stats = pass_stats.clone();
        std::thread::Builder::new()
            .name("rt-drop-reporter".into())
            .spawn(move || loop {
                std::thread::sleep(DROP_REPORT_INTERVAL);
                let cap = cap_dropped.swap(0, Ordering::Relaxed);
                let pass = pass_dropped.swap(0, Ordering::Relaxed);
                if cap > 0 || pass > 0 {
                    log::warn!(
                        "przepełnienie ringów RT w ostatnich {}s: capture {cap} próbek, \
                         passthrough {pass} próbek — tor AI/odtwarzanie nie nadąża",
                        DROP_REPORT_INTERVAL.as_secs()
                    );
                }
                // Etap 1a: czysty pomiar. Ta linia ma dać TWARDĄ liczbę pod
                // decyzję o regulatorze zaległości i o rozmiarze "prime"
                // (main.rs) — dziś obie stoją na wyprowadzeniu, nie na pomiarze.
                //
                // DEBUG, nie INFO: to przyrząd na sesję pomiarową, a nie
                // zachowanie domyślne programu. Na INFO byłoby to ~720 linii
                // na godzinę wpisywanych przez launcher na dysk, bez sposobu
                // wyłączenia innego niż ucięcie wszystkich INFO. Odsłuch
                // pomiarowy: uruchom z `-v`.
                match pass_stats.take() {
                    Some(s) if s.quantum_frames > 0 => log::debug!(
                        "passthrough: zaległość min {:.1} / max {:.1} / ost. {:.1} ms \
                         (kwant {} ramek = {:.1} ms, {} cykli/{}s)",
                        pass_samples_to_ms(s.min_samples),
                        pass_samples_to_ms(s.max_samples),
                        pass_samples_to_ms(s.last_samples),
                        s.quantum_frames,
                        s.quantum_frames as f32 / RATE as f32 * 1000.0,
                        s.cycles,
                        DROP_REPORT_INTERVAL.as_secs()
                    ),
                    Some(s) => log::debug!(
                        "passthrough: zaległość min {:.1} / max {:.1} / ost. {:.1} ms \
                         (kwant NIEZNANY — graf ani razu nie podał `requested`, obowiązuje \
                         FALLBACK_FRAMES={FALLBACK_FRAMES} ramek; {} cykli/{}s)",
                        pass_samples_to_ms(s.min_samples),
                        pass_samples_to_ms(s.max_samples),
                        pass_samples_to_ms(s.last_samples),
                        s.cycles,
                        DROP_REPORT_INTERVAL.as_secs()
                    ),
                    // NIE pisz tu „nieprzypięty do sprzętu" — to nieprawda.
                    // Odlinkowany węzeł DALEJ jest wołany („normally idle nodes
                    // keep processing", man 7 pipewire-props), a suspend go nie
                    // dotyka: monitors/suspend-node.lua filtruje `Audio/*`,
                    // a my mamy `Stream/Output/Audio`. Zerowa liczba cykli
                    // znaczy zawieszony węzeł albo stojący serwer — utratę celu
                    // wykrywa `verdict` przez node.dont-fallback, nie ta linia.
                    None => log::debug!(
                        "passthrough: ani jednego cyklu odtwarzania w ostatnich {}s \
                         — węzeł zawieszony albo serwer stanął",
                        DROP_REPORT_INTERVAL.as_secs()
                    ),
                }
            })?;
    }

    log::info!(
        "węzeł \"Nacelle Translator (PL)\" gotowy (node.name={SINK_NODE_NAME}) — skieruj do \
         niego dźwięk: `wpctl set-default <ID>`, gdzie ID bierzesz z `nacelle-translator \
         devices`. UWAGA: aplet głośności KDE UKRYWA ten węzeł na liście urządzeń \
         (node.virtual=true + filterVirtualDevices w plasma-pa), więc w samym aplecie go \
         nie znajdziesz — działa za to przenoszenie aplikacji w Ustawienia → Dźwięk → \
         Aplikacje. Ctrl+C kończy"
    );
    mainloop.run();

    // KOLEJNOŚĆ JEST ISTOTNA. `disconnect()` sam wywołuje `state_changed`
    // z `Unconnected`, a to jest werdykt fatalny (patrz `verdict`) — gdyby
    // odczyt `fatal` był po rozłączeniu, KAŻDY czysty Ctrl+C kończyłby się
    // kodem 1 i komunikatem o utracie celu. Dodatkowo `sink` rozłącza się jako
    // drugi, więc NADPISAŁBY prawdziwą przyczynę zapisaną wcześniej przez
    // `playback`, maskując realną awarię celu odtwarzania komunikatem o sinku.
    let fatal_msg = fatal.borrow_mut().take();

    play_stream.disconnect()?;
    sink_stream.disconnect()?;
    drop(default_watch);

    if let Some(msg) = fatal_msg {
        bail!(msg);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w1_pomiar_bez_cykli_nie_raportuje() {
        let s = PassStats::new();
        assert_eq!(s.take(), None);
    }

    #[test]
    fn w2_pomiar_zbiera_min_max_i_ostatni() {
        let s = PassStats::new();
        s.observe(4096, 1024);
        s.observe(100, 1024);
        s.observe(2048, 1024);
        let snap = s.take().expect("były cykle");
        assert_eq!(snap.min_samples, 100);
        assert_eq!(snap.max_samples, 4096);
        assert_eq!(snap.last_samples, 2048);
        assert_eq!(snap.quantum_frames, 1024);
        assert_eq!(snap.cycles, 3);
    }

    #[test]
    fn w3_okno_min_max_zeruje_sie_po_odczycie() {
        let s = PassStats::new();
        s.observe(9999, 512);
        s.take().unwrap();
        // nowe okno nie może odziedziczyć maksimum z poprzedniego
        s.observe(64, 512);
        let snap = s.take().unwrap();
        assert_eq!(snap.min_samples, 64);
        assert_eq!(snap.max_samples, 64);
        assert_eq!(snap.cycles, 1);
    }

    fn err(m: &str) -> StreamState {
        StreamState::Error(m.to_string())
    }

    #[test]
    fn w5_blad_sesyjny_bez_zmiany_stanu_to_tylko_ostrzezenie() {
        // sendClientError od WirePlumbera: zdarzenie Error, ale stream.state()
        // nadal Streaming/Paused — proces ma to przeżyć
        assert_eq!(
            verdict(&err("no target node available"), false),
            Verdict::Warn
        );
        assert_eq!(verdict(&err("target not found"), false), Verdict::Warn);
    }

    #[test]
    fn w6_prawdziwy_blad_strumienia_zostaje_fatalny() {
        // pw_stream_set_error: stan strumienia FAKTYCZNIE jest Error
        assert_eq!(verdict(&err("format rejected"), true), Verdict::Fatal);
        assert_eq!(verdict(&err("no buffers"), true), Verdict::Fatal);
    }

    #[test]
    fn w7_unconnected_zawsze_fatalny() {
        // Zdarzenie Unconnected powstaje wyłącznie w proxy_removed /
        // proxy_destroy / on_core_error(-EPIPE) — a stan początkowy tej samej
        // nazwy NIE jest emitowany. Bramka „dopiero po Streaming/Paused"
        // uzależniała przeżycie procesu od wyścigu między sendClientError
        // a request_destroy przy pierwszym linkowaniu.
        assert_eq!(verdict(&StreamState::Unconnected, false), Verdict::Fatal);
        assert_eq!(verdict(&StreamState::Unconnected, true), Verdict::Fatal);
    }

    #[test]
    fn w8_stany_robocze_ignorowane() {
        for s in [
            StreamState::Connecting,
            StreamState::Paused,
            StreamState::Streaming,
        ] {
            assert_eq!(verdict(&s, false), Verdict::Ignore, "{s:?}");
            // nawet gdy stream.state() zdąży już pokazać Error, samo przejście
            // w stan roboczy nie jest powodem do zabijania procesu
            assert_eq!(verdict(&s, true), Verdict::Ignore, "{s:?}");
        }
    }

    #[test]
    fn w9_kwant_zero_nie_udaje_pomiaru() {
        // adapter bez sugestii kwantu (req=0) NIE może wygrać z realnym
        // pomiarem tylko dlatego, że wypadł ostatni w oknie
        let s = PassStats::new();
        s.observe(2048, 1024);
        s.observe(2048, 0);
        let snap = s.take().unwrap();
        assert_eq!(snap.quantum_frames, 1024);
    }

    #[test]
    fn w10_kwant_nieznany_gdy_graf_nigdy_go_nie_podal() {
        let s = PassStats::new();
        s.observe(512, 0);
        s.observe(512, 0);
        let snap = s.take().unwrap();
        // 0 = „nie wiem", a wydruk ma to nazwać wprost, zamiast pokazywać
        // „kwant 0 ramek = 0.0 ms" jako zmierzoną liczbę
        assert_eq!(snap.quantum_frames, 0);
        assert_eq!(snap.cycles, 2);
    }

    #[test]
    fn w11_kwant_nalezy_do_okna() {
        let s = PassStats::new();
        s.observe(100, 2048);
        s.take().unwrap();
        s.observe(100, 0);
        // nowe okno nie może odziedziczyć kwantu po poprzednim
        assert_eq!(s.take().unwrap().quantum_frames, 0);
    }

    #[test]
    fn b1_bramka_zamknieta_odcina_oba_tory() {
        let g = TranslateGate::new(false);
        assert!(!g.feeds_ai(), "zamknięta bramka nie może karmić toru AI");
        assert!(!g.plays_tts(), "zamknięta bramka nie może grać lektora");
    }

    #[test]
    fn b2_bramka_otwarta_przepuszcza_oba_tory() {
        let g = TranslateGate::new(true);
        assert!(g.feeds_ai());
        assert!(g.plays_tts());
    }

    #[test]
    fn b3_obie_decyzje_zawsze_zgodne() {
        // NIEZMIENNIK: nigdy „karm AI, nie graj lektora" (praca GPU bez
        // efektu) ani „graj lektora, nie karm AI" (ducking pod resztkę mowy
        // z ringu, bez powiązanego oryginału). Rozjazd nie wywala programu,
        // więc bez tego testu byłby cichy.
        for on in [false, true] {
            let g = TranslateGate::new(on);
            assert_eq!(g.feeds_ai(), g.plays_tts(), "rozjazd bramki dla on={on}");
        }
    }

    #[test]
    fn b4_domyslnie_bramka_jest_zamknieta() {
        // Repozytorium jest publiczne: `git clone` + `cargo run` nie może
        // zacząć mielić całego dźwięku systemu na GPU.
        let cfg = crate::config::Config::default();
        assert!(!cfg.audio.translate);
        assert!(!TranslateGate::new(cfg.audio.translate).feeds_ai());
    }

    #[test]
    fn b5_opis_trybu_nazywa_klucz_konfiguracji() {
        // Log startowy ma dać się przeczytać BEZ zaglądania do kodu: przy
        // wyłączonym tłumaczeniu musi paść nazwa klucza, którym się je włącza.
        let off = TranslateGate::new(false).describe();
        assert!(off.contains("translate"), "{off}");
        assert!(off.contains("WYŁĄCZONE"), "{off}");
        assert!(TranslateGate::new(true).describe().contains("WŁĄCZONE"));
    }

    #[test]
    fn w4_przeliczenie_probek_na_ms() {
        // 4096 ramek stereo = 8192 próbek f32 @48 kHz = 85,333 ms
        assert!((pass_samples_to_ms(4096 * CHANNELS as u64) - 85.333).abs() < 0.01);
        // jeden kwant 1024 ramek = 21,333 ms
        assert!((pass_samples_to_ms(1024 * CHANNELS as u64) - 21.333).abs() < 0.01);
        assert_eq!(pass_samples_to_ms(0), 0.0);
    }
}
