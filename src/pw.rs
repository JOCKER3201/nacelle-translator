//! Graf PipeWire: wirtualny sink (media.class=Audio/Sink) + strumień
//! odtwarzania wycelowany w sprzętowe urządzenie. Callbacki RT wymieniają
//! próbki z resztą programu wyłącznie przez lock-free ringbuffery SPSC.
//!
//! Ochrona przed pętlą (gdy nasz sink jest domyślnym urządzeniem):
//!  1. target.object = node.name sprzętu (PipeWire ≥ 0.3.64 przyjmuje nazwę),
//!  2. StreamFlags::DONT_RECONNECT — po zniknięciu celu strumień pada,
//!     zamiast wracać do domyślnego sinka (czyli naszego),
//!  3. node.link-group o tej samej wartości na obu strumieniach
//!     (wzorzec z module-loopback),
//!  4. przy automatycznym wyborze celu odfiltrowujemy własne węzły I każdy
//!     wirtualny sink obcego pochodzenia (akceptujemy tylko sprzęt ALSA/BT).
//!
//! Odporność: oba strumienie rejestrują `state_changed` — błąd albo
//! zniknięcie węzła (DONT_RECONNECT niszczy węzeł playbacku, gdy cel
//! przepada) kończy program głośno zamiast zostawiać go jako cichego
//! zombie. To samo dotyczy śmierci dowolnego wątku toru AI — sygnalizuje
//! ją `health_rx` przekazany z `pipeline::spawn`.

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
fn read_default_sink_name(mainloop: &pw::main_loop::MainLoopRc, core: &pw::core::CoreRc) -> Option<String> {
    let registry = match core.get_registry_rc() {
        Ok(r) => r,
        Err(e) => {
            log::debug!("nie mogę pobrać rejestru do odczytu domyślnego wyjścia: {e:#}");
            return None;
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
                        move |_subject, key, _type_, value| {
                            let target = match key {
                                Some("default.configured.audio.sink") => Some(&configured),
                                Some("default.audio.sink") => Some(&active),
                                _ => None,
                            };
                            if let (Some(slot), Some(v)) = (target, value) {
                                if let Ok(j) = serde_json::from_str::<serde_json::Value>(v) {
                                    if let Some(n) = j["name"].as_str() {
                                        *slot.borrow_mut() = Some(n.to_string());
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
        return None;
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
        return None;
    }

    configured.take().or_else(|| active.take())
}

/// Enumeracja węzłów Audio/Sink + odczyt aktualnie skonfigurowanego
/// domyślnego wyjścia, w jednej sesji (podkomendy `devices` i `check`).
pub fn discover_sinks() -> Result<(Vec<SinkInfo>, Option<String>)> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;
    let sinks = enumerate_sinks(&mainloop, &core)?;
    let default = read_default_sink_name(&mainloop, &core);
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
}

struct PlayState {
    pass_cons: HeapCons<f32>,
    tts_cons: HeapCons<f32>,
    pass_scratch: Vec<f32>,
    tts_scratch: Vec<f32>,
    duck: DuckParams,
    gain: f32,
    hold: u32,
}

/// Współdzielony stan fatalny: ustawiany z callbacków `state_changed`/`error`
/// (main-loop, nie RT) i ze strażnika wątków toru AI; po `mainloop.run()`
/// zamieniany na `Err`, żeby proces zakończył się głośno zamiast wisieć.
type Fatal = Rc<RefCell<Option<String>>>;

fn watch_stream<D>(
    name: &'static str,
    fatal: &Fatal,
    ml: &pw::main_loop::MainLoopRc,
) -> impl FnMut(&pw::stream::Stream, &mut D, StreamState, StreamState) {
    let fatal = fatal.clone();
    let ml = ml.clone();
    let was_active = Cell::new(false);
    move |_stream, _ud, old, new| {
        log::debug!("{name}: stan {old:?} -> {new:?}");
        match new {
            StreamState::Error(msg) => {
                *fatal.borrow_mut() = Some(format!("{name}: błąd strumienia: {msg}"));
                ml.quit();
            }
            StreamState::Streaming | StreamState::Paused => was_active.set(true),
            StreamState::Unconnected if was_active.get() => {
                *fatal.borrow_mut() = Some(format!(
                    "{name}: węzeł zniknął z grafu (cel odtwarzania usunięty?)"
                ));
                ml.quit();
            }
            _ => {}
        }
    }
}

/// Buduje graf i blokuje w pętli głównej do SIGINT/SIGTERM, błędu strumienia
/// albo śmierci dowolnego wątku toru AI (`health_rx`).
pub fn run_graph(
    output_device: Option<&str>,
    rings: RtRings,
    duck: DuckParams,
    health_rx: crossbeam_channel::Receiver<String>,
) -> Result<()> {
    pw::init();

    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;

    let sinks = enumerate_sinks(&mainloop, &core)?;
    let default_sink = read_default_sink_name(&mainloop, &core);
    let target = pick_output(&sinks, output_device, default_sink.as_deref())?;
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
    };

    let _sink_listener = sink_stream
        .add_local_listener_with_user_data(sink_state)
        .state_changed(watch_stream("sink", &fatal, &mainloop))
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
                // downmix mono dla toru AI; pełny ring => nadmiar przepada
                let frames = nsamples / CHANNELS;
                for f in 0..frames {
                    st.mono[f] = (st.stereo[f * 2] + st.stereo[f * 2 + 1]) * 0.5;
                }
                let pushed = st.cap_prod.push_slice(&st.mono[..frames]);
                if pushed < frames {
                    st.cap_dropped
                        .fetch_add((frames - pushed) as u64, Ordering::Relaxed);
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
    };

    let _play_listener = play_stream
        .add_local_listener_with_user_data(play_state)
        .state_changed(watch_stream("playback", &fatal, &mainloop))
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
                    let got_tts = st.tts_cons.pop_slice(&mut st.tts_scratch[..n]);

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

    // ---------- okresowy raport przepełnień ringów RT ----------
    {
        let cap_dropped = cap_dropped.clone();
        let pass_dropped = pass_dropped.clone();
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
            })?;
    }

    log::info!(
        "węzeł \"Nacelle Translator (PL)\" gotowy — ustaw go jako wyjście dźwięku \
         (wpctl set-default albo ustawienia KDE); Ctrl+C kończy"
    );
    mainloop.run();

    play_stream.disconnect()?;
    sink_stream.disconnect()?;

    if let Some(msg) = fatal.borrow_mut().take() {
        bail!(msg);
    }
    Ok(())
}
