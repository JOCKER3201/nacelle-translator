//! Graf PipeWire: PRZELOTKA w torze dźwięku — wirtualny sink
//! (media.class=Audio/Sink) plus strumień odtwarzania, oba w jednej grupie
//! `node.link-group`, zgłoszone WirePlumberowi jako inteligentny filtr
//! (`filter.smart`). Callbacki RT wymieniają próbki z resztą programu
//! wyłącznie przez lock-free ringbuffery SPSC.
//!
//! MODEL: przelotowy łącznik z tłumaczem w środku. Użytkownik wybiera swój
//! sprzęt w KDE tak jak zawsze, a przelotka siedzi w torze i PODĄŻA za tym
//! wyborem. Programu NIE wybiera się jako urządzenia (`node.virtual=true`,
//! aplet głośności go nie pokazuje — i dobrze, o to chodzi) i program NIE
//! wybiera urządzenia za użytkownika. Zmiana wyjścia w aplecie to ścieżka
//! NORMALNA: demon ma ją przeżyć dowolną liczbę razy, bez restartu.
//!
//! Jak to działa w WirePlumberze 0.5.12 (sprawdzone w źródłach na tej
//! maszynie, nie w dokumentacji):
//!  - `filter.smart=true` na węźle MAIN (tym bez "Stream" w media.class)
//!    włącza cały mechanizm: lib/filter-utils.lua `rescanFilters` wciąga nas
//!    do tablicy filtrów, a `getFilterSmart` bez tego klucza zwraca false
//!    i mechanizm śpi. Węzeł STREAM (`Stream/Output/Audio`, ten sam
//!    link-group) jest dobierany do pary automatycznie.
//!  - BRAK `filter.smart.target` jest tu WARTOŚCIĄ, nie brakiem: robi z nas
//!    filtr „bezcelowy" (`getFilterSmartTargetless`, filter-utils.lua:161).
//!    Aplikacje idące do domyślnego wyjścia trafiają wtedy w nas
//!    (get-filter-from-target.lua sięga po `get_filter_from_target(dir, mt,
//!    nil)`, gałąź `target == nil and v.targetless`) — to wzorzec
//!    EasyEffects. Ustawienie `filter.smart.target` przypięłoby nas na
//!    sztywno do jednego sprzętu, czyli dokładnie odwrotnie do celu.
//!  - nasz własny STREAM nie ma żadnego zdefiniowanego celu, więc
//!    find-filter-target.lua oddaje sterowanie, a find-default-target.lua
//!    linkuje go do AKTUALNIE domyślnego sinka.
//!  - przy zmianie domyślnego wyjścia rescan.lua (interest na
//!    `metadata-changed` / `default.audio.sink`) planuje ponowne
//!    przetworzenie, a prepare-link.lua — dopóki strumień JEST
//!    „reconnect" — zrywa stary link i robi nowy („moving to new target").
//!    Bez restartu procesu.
//!
//! CZEGO TU CELOWO NIE MA I DLACZEGO:
//!  - `target.object` na strumieniu: find-defined-target.lua ustawia wtedy
//!    `has_defined_target=true` i przypina nas na stałe do JEDNEGO sprzętu.
//!  - `StreamFlags::DONT_RECONNECT`: prepare-link.lua:73-76
//!    (`if not reconnect and si_flags.was_handled then target = nil`) —
//!    po pierwszym zlinkowaniu filtr NIGDY by się nie przeniósł na nowe
//!    wyjście. To jest wprost sprzeczne z modelem przelotki.
//!  - `node.dont-fallback`: przy `filter.smart` ten klucz jest dla nas
//!    ZABÓJCZY. find-filter-target.lua wykrywa nas jako smart filter, dla
//!    filtra bezcelowego nie ma celu do zwrócenia i wchodzi w gałąź
//!    `is_smart_filter and dont_fallback`: `sendClientError` +
//!    `node:request_destroy()`. Czyli własnoręczne niszczenie własnego
//!    węzła przy każdym rescanie.
//!  - własnego wyboru urządzenia (dawne `pick_output`): wybiera
//!    WirePlumber, my nie mamy w tym głosu ani potrzeby.
//!  - `node.passive` na strumieniu PRZY WŁĄCZONYM TŁUMACZENIU: przy
//!    otwartej bramce nie jesteśmy wyłącznie filtrem, tylko także źródłem
//!    dźwięku (lektor), a pasywny węzeł nie budzi uśpionego wyjścia.
//!    Szczegóły przy `play_props`.
//!
//! CZEGO WIREPLUMBEROWI ZABRANIAMY WPROST:
//!  - `state.restore-target = false` na OBU węzłach: bez tego
//!    node/state-stream.lua przywraca zapamiętany cel przez
//!    `metadata:set(..., "target.object", ...)`, a find-defined-target.lua
//!    robi z tego `has_defined_target = true` — czyli przypięcie do jednego
//!    sprzętu wraca tylnymi drzwiami i przeżywa restart.
//!  - `state.restore-props = false` na OBU węzłach: zapamiętane wyciszenie
//!    któregokolwiek z nich ścisza CAŁY system przy każdym starcie, w
//!    miejscu, którego użytkownik nie widzi w aplecie. Tak samo robi wzorzec
//!    valve-galileo (wireplumber.conf.d/stream.conf, dla obu węzłów pary).
//!
//! Ochrona przed pętlą sprzężenia — co zostało i co faktycznie działa:
//!  1. `node.link-group` o tej samej wartości na obu węzłach. To jedyny
//!     zamek, który nie zależy od żadnej naszej decyzji: linking-utils.lua
//!     `canLinkGroupCheck` odmawia linkowania węzłów o tej samej wartości
//!     i rekurencyjnie (do 8 hopów) wykrywa pętle pośrednie. Sprawdzany
//!     przez `canLink` na KAŻDEJ ścieżce wyboru celu, także w
//!     find-default-target.lua.
//!  2. filter-utils.lua pomija cele mające `node.link-group` ORAZ
//!     `filter.smart` (getFilterSmartTarget:139-142) — żaden inny
//!     inteligentny filtr nie wskaże nas jako celu i my nie wskażemy jego.
//!  3. gdy użytkownik ustawi NASZ węzeł jako domyślne wyjście, domyślnym
//!     celem naszego strumienia stajemy się my sami i `canLink` odmawia.
//!     To NIE kończy się ciszą — i to jest ważne, bo poprzednia wersja tego
//!     komentarza (i cała diagnostyka pod nim) twierdziła inaczej.
//!     find-default-target.lua nie robi `stop_processing`, więc sterowanie
//!     leci dalej do linking/find-best-target.lua (zarejestrowany
//!     w wireplumber.conf, `after = { ... find-default-target }`), a ten
//!     iteruje po węzłach `item.node.type = device`, POMIJA inteligentne
//!     filtry (czyli nas, linie 60-65) i wybiera najlepszy sprzętowy sink po
//!     `priority.session`. Nasz strumień linkuje się więc do sprzętu, a że
//!     aplikacje i tak trafiają w nas — dźwięk PRZECHODZI.
//!     Ten stan jest zatem mylący, a nie zabójczy: mamy o nim ostrzec
//!     (`warn_if_default_is_us`), ale nie wolno nam nazywać go awarią ani
//!     zwracać za niego kodu błędu z `check`.
//!  4. o tym, dokąd trafia nasz strumień, decyduje AKTYWNE domyślne wyjście
//!     (`default.audio.sink`), a nie zapamiętany wybór użytkownika
//!     (`default.configured.audio.sink`). Łańcuch: find-default-target →
//!     linking-utils `findDefaultLinkable` → common-utils `getDefaultNode` →
//!     plugin `default-nodes-api` / `get-default-node`, czyli wartość klucza
//!     AKTYWNEGO. Oba klucze potrafią się rozjechać (np. gdy zapamiętane
//!     urządzenie akurat nie istnieje), więc czujnik obserwuje OBA i ocenia
//!     je OSOBNO — sklejenie ich przez `or_else` maskowało stan aktywny.
//!
//! Odporność: oba strumienie rejestrują `state_changed`. Prawdziwy błąd
//! strumienia (`stream.state()` faktycznie w Error) albo przejście
//! w `Unconnected` (serwer zniszczył węzeł albo zerwało się połączenie)
//! kończy program głośno zamiast zostawiać go jako cichego zombie. Rutynowy
//! błąd sesyjny od WirePlumbera — dostarczany jako zdarzenie `Error` BEZ
//! zmiany stanu strumienia — jest tylko logowany, z dławieniem; szczegóły
//! przy `verdict`. W modelu przelotki to rozróżnienie jest WAŻNIEJSZE niż
//! wcześniej: chwilowy brak celu (przełączanie profilu karty, uśpienie
//! słuchawek, zmiana urządzenia w aplecie) jest teraz ścieżką NORMALNĄ
//! i nie ma prawa kończyć procesu. Śmierć wątku toru AI sygnalizuje
//! `health_rx` przekazany z `pipeline::spawn`.

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
/// `filter.smart.name` — nazwa filtra w tablicy WirePlumbera. Bez niej
/// filter-utils.lua bierze `node.link-group` (getFilterSmartName:74), więc
/// technicznie jest opcjonalna; podajemy ją jawnie, bo to po niej inne filtry
/// (i my sami, gdyby kiedyś doszło `filter.smart.before/after`) identyfikują
/// nas w kolejności łańcucha.
const FILTER_SMART_NAME: &str = "nacelle-translator";
/// wspólna grupa obu węzłów — dla WirePlumbera dowód, że jesteśmy filtrem,
/// i jednocześnie zamek `canLinkGroupCheck` przeciw pętli sprzężenia
const LINK_GROUP: &str = "nacelle-translator";

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
    /// `false` = tor AI nie dostaje ani jednej próbki. Zdanie „GPU stoi" jest
    /// prawdziwe dopiero dzięki temu, że `cmd_run` przy zamkniętej bramce
    /// w ogóle nie woła `pipeline::spawn` — sama bramka w RT odcina tylko
    /// dopływ próbek i model siedziałby w VRAM mimo niej.
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
            "tłumaczenie WYŁĄCZONE ([audio].translate = false): węzeł jest czystą przelotką — \
             tor AI nie jest budowany (whisper nie ładuje się do VRAM, piper nie startuje, \
             silnik tłumaczenia nie jest potrzebny) i nie dostaje ani jednej próbki. \
             Włącz kluczem [audio].translate = true w nacelle-translator.toml"
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

/// [RT] Downmiks do mono i podanie go torowi AI. Zwraca liczbę próbek, które
/// NIE zmieściły się w ringu (0 = wszystko przeszło albo bramka zamknięta).
///
/// Wydzielone z callbacku sinka wyłącznie po to, żeby dało się to
/// PRZETESTOWAĆ. Twierdzenie commita bramkowego — „przy zamkniętej bramce
/// pomijamy CAŁOŚĆ, także sam downmiks" — żyło wcześniej w domknięciu
/// nieosiągalnym z testów, więc pilnowało go tylko czyjeś oko.
///
/// `stereo` jest interleaved (FL, FR, FL, FR, …); `mono` musi mieć miejsce na
/// `stereo.len() / CHANNELS` próbek.
#[inline]
fn feed_ai(
    gate: TranslateGate,
    stereo: &[f32],
    mono: &mut [f32],
    cap_prod: &mut HeapProd<f32>,
) -> usize {
    if !gate.feeds_ai() {
        return 0;
    }
    let frames = stereo.len() / CHANNELS;
    for f in 0..frames {
        mono[f] = (stereo[f * 2] + stereo[f * 2 + 1]) * 0.5;
    }
    frames - cap_prod.push_slice(&mono[..frames])
}

/// [RT] Zmieszanie jednego kawałka: oryginał (przyciszany pod lektorem)
/// plus lektor. Wszystko interleaved stereo poza `tts`, który jest mono.
///
/// `gain` i `hold` to STAN CIĄGŁY obwiedni duckingu — przechodzą przez
/// granice kawałków i cykli, więc są przekazywane przez `&mut`, a nie
/// zwracane.
///
/// Bramka jest sprawdzana także TUTAJ, mimo że wołający i tak nie drenuje
/// ringu TTS przy zamkniętej bramce. To nie jest podwójna robota, tylko
/// jedyny sposób, żeby dało się przetestować zdanie „ducking ani razu się
/// nie odpali": test podaje niezerowy `tts` przy zamkniętej bramce
/// i sprawdza, że wyjście jest bit w bit oryginałem.
#[inline]
fn mix_chunk(
    gate: TranslateGate,
    duck: &DuckParams,
    gain: &mut f32,
    hold: &mut u32,
    pass: &[f32],
    tts: &[f32],
    out: &mut [f32],
) {
    let tts: &[f32] = if gate.plays_tts() { tts } else { &[] };
    let frames = out.len() / CHANNELS;
    let pass_frames = pass.len() / CHANNELS;
    for j in 0..frames {
        let has_tts = j < tts.len();
        // Duck zawsze, gdy TTS gra TERAZ; poza tym trzymaj przyciszenie
        // jeszcze `hold_frames` po jego końcu. (Warunek musi sprawdzać
        // `has_tts` wprost — samo "hold > 0" ustawione w tej samej iteracji,
        // w której hold właśnie przypisano, prowadziłoby przy
        // hold_frames == 0 do duckingu, który nigdy się nie uruchamia.)
        let target_gain = if has_tts {
            *hold = duck.hold_frames;
            duck.duck_gain
        } else if *hold > 0 {
            *hold -= 1;
            duck.duck_gain
        } else {
            duck.pass_gain
        };
        let coef = if target_gain < *gain {
            duck.attack_coef
        } else {
            duck.release_coef
        };
        *gain += (target_gain - *gain) * coef;

        let (pl, pr) = if j < pass_frames {
            (pass[j * 2], pass[j * 2 + 1])
        } else {
            (0.0, 0.0) // underrun passthrough => cisza
        };
        let t = if has_tts { tts[j] * duck.tts_gain } else { 0.0 };
        out[j * 2] = soft_clip(pl * *gain + t);
        out[j * 2 + 1] = soft_clip(pr * *gain + t);
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
) -> Result<(Vec<SinkInfo>, Option<String>)> {
    let registry = core.get_registry_rc()?;
    let sinks = Rc::new(RefCell::new(Vec::<SinkInfo>::new()));
    // Menedżer sesji: przy okazji tej samej enumeracji, bo to od NIEGO zależy,
    // czy `filter.smart` w ogóle coś znaczy (patrz `session_manager`).
    let session_mgr: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let core_err: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));

    let _reg_listener = registry
        .add_listener_local()
        .global({
            let sinks = sinks.clone();
            let session_mgr = session_mgr.clone();
            move |g| {
                if g.type_ == pw::types::ObjectType::Client {
                    if let Some(name) = g.props.and_then(|p| p.get("application.name")) {
                        // „WirePlumber [export]" to ten sam proces — bierzemy
                        // pierwszą nazwę i nie doklejamy duplikatów.
                        if session_mgr.borrow().is_none()
                            && (name.starts_with("WirePlumber")
                                || name.contains("media-session"))
                        {
                            *session_mgr.borrow_mut() = Some(name.to_string());
                        }
                    }
                    return;
                }
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
    Ok((sinks.take(), session_mgr.take()))
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
) -> (DefaultSinks, DefaultSinkWatch) {
    // Uzbrajane dopiero po odczycie startowym (`arm()`), żeby początkowy
    // zrzut metadanych nie udawał zmiany zrobionej przez użytkownika.
    let armed: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    // Stos poprzednich wyborów czytamy RAZ, przy starcie: w callbacku
    // metadanych nie ma po co dotykać dysku, a treść i tak się nie zmienia
    // w sposób, który by nam pomógł (WirePlumber zapisuje ten plik leniwie).
    let state_raw: Rc<Option<String>> = Rc::new(read_wp_state());
    let registry = match core.get_registry_rc() {
        Ok(r) => r,
        Err(e) => {
            log::debug!("nie mogę pobrać rejestru do odczytu domyślnego wyjścia: {e:#}");
            return (DefaultSinks::default(), DefaultSinkWatch::inert(armed));
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
            let armed = armed.clone();
            let state_raw = state_raw.clone();
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
                        let armed = armed.clone();
                        let state_raw = state_raw.clone();
                        move |_subject, key, _type_, value| {
                            // OBA klucze, nie tylko `configured`: o linkowaniu
                            // decyduje AKTYWNY `default.audio.sink` (nagłówek
                            // pliku, punkt 4), a WirePlumber potrafi go zmienić
                            // sam, nie dotykając zapamiętanego wyboru.
                            let (slot, kind) = match key {
                                Some("default.configured.audio.sink") => {
                                    (&configured, DefaultSlot::Configured)
                                }
                                Some("default.audio.sink") => (&active, DefaultSlot::Active),
                                _ => return 0,
                            };
                            let Some(v) = value else { return 0 };
                            let Ok(j) = serde_json::from_str::<serde_json::Value>(v) else {
                                return 0;
                            };
                            let Some(n) = j["name"].as_str() else { return 0 };
                            // Serwer potrafi przysłać tę samą wartość ponownie
                            // (np. przy rescanie). Bez porównania z poprzednią
                            // log dostawałby duplikaty zdarzenia, którego nie
                            // było.
                            if slot.borrow().as_deref() == Some(n) {
                                return 0;
                            }
                            *slot.borrow_mut() = Some(n.to_string());
                            if !armed.get() {
                                return 0;
                            }
                            if warn_if_default_is_us(kind, Some(n), state_raw.as_deref()) {
                                return 0;
                            }
                            // Zmiana urządzenia w aplecie KDE to ścieżka
                            // NORMALNA — przelotka przepina się sama
                            // (rescan.lua + prepare-link.lua, patrz nagłówek
                            // pliku). Logujemy ją, bo bez tego wpisu nie da
                            // się z logu sesji sprawdzić, że demon przeżył
                            // przełączenie — a to jest wymaganie twarde,
                            // nie ciekawostka.
                            if kind == DefaultSlot::Configured {
                                log::info!(
                                    "domyślne wyjście dźwięku zmieniono na \"{n}\" \
                                     — przelotka podąża za tym wyborem sama, bez restartu"
                                );
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
        return (DefaultSinks::default(), DefaultSinkWatch::inert(armed));
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
        return (DefaultSinks::default(), DefaultSinkWatch::inert(armed));
    }

    // `borrow().clone()` zamiast `take()`: podpięcie ma żyć dalej i porównywać
    // przyszłe zmiany, więc nie wolno opróżnić slotów.
    //
    // Oba sloty ODDZIELNIE. Dawne `configured.or_else(active)` maskowało
    // dokładnie ten stan, który jest groźny: zapamiętany wybór wskazuje
    // sprzęt, a aktywnym domyślnym wyjściem (tym, po które sięga
    // `findDefaultLinkable`) jesteśmy my.
    let name = DefaultSinks {
        configured: configured.borrow().clone(),
        active: active.borrow().clone(),
    };
    let watch = DefaultSinkWatch {
        armed,
        state_raw,
        _md: metadata_bound.take(),
        _md_listener: metadata_listener.take(),
    };
    (name, watch)
}

/// Który z dwóch kluczy metadanych „default" opisuje daną nazwę.
///
/// Rozróżnienie jest istotne, bo klucze znaczą co innego i potrafią się
/// rozjechać (patrz nagłówek pliku, punkt 4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DefaultSlot {
    /// `default.configured.audio.sink` — zapamiętany, świadomy wybór
    /// użytkownika. Może wskazywać urządzenie, którego w tej chwili nie ma.
    Configured,
    /// `default.audio.sink` — to, po co WirePlumber faktycznie sięga przy
    /// wyborze celu (`findDefaultLinkable` → `get-default-node`). Ten klucz
    /// WirePlumber ustawia też SAM, bez udziału użytkownika.
    Active,
}

/// Oba domyślne wyjścia odczytane z metadanych „default", każde osobno.
///
/// Nie ma tu metody „to jedno prawdziwe domyślne wyjście", bo takiej wartości
/// nie ma: dla routingu liczy się `active`, dla intencji użytkownika
/// `configured`, a sklejanie ich w jedno zamaskowało już raz stan, w którym
/// aktywnym wyjściem był nasz własny węzeł.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DefaultSinks {
    pub configured: Option<String>,
    pub active: Option<String>,
}

impl DefaultSinks {
    /// Wyjście, do którego faktycznie pojedzie dźwięk. `configured` służy tu
    /// wyłącznie jako awaryjne źródło nazwy, gdy serwer nie przysłał jeszcze
    /// klucza aktywnego.
    pub fn effective(&self) -> Option<&str> {
        self.active.as_deref().or(self.configured.as_deref())
    }
}

/// Żywe podpięcie do obiektu metadanych "default" — WYŁĄCZNIE do odczytu.
///
/// Dopóki uchwyty żyją, serwer woła nasz `property()` przy każdej zmianie
/// domyślnego wyjścia. Program niczego tu nie zapisuje ani nie przełącza:
/// przepięciem zajmuje się WirePlumber, a my tylko to odnotowujemy —
/// i ostrzegamy w jedynym stanie, którego nie da się rozwiązać po naszej
/// stronie (domyślnym wyjściem jesteśmy MY sami).
struct DefaultSinkWatch {
    /// dopóki `false`, callback milczy — inaczej początkowy zrzut metadanych
    /// wyglądałby jak przełączenie urządzenia zrobione przez użytkownika
    armed: Rc<Cell<bool>>,
    /// stos poprzednich wyborów z pliku stanu WirePlumbera (odczyt raz)
    state_raw: Rc<Option<String>>,
    _md: Option<pw::metadata::Metadata>,
    _md_listener: Option<pw::metadata::MetadataListener>,
}

impl DefaultSinkWatch {
    /// wariant bez podpięcia (nie udało się dobić do metadanych) — wszystkie
    /// metody działają, tylko nikt nigdy nie zawoła `property()`
    fn inert(armed: Rc<Cell<bool>>) -> Self {
        Self {
            armed,
            state_raw: Rc::new(None),
            _md: None,
            _md_listener: None,
        }
    }

    /// Stos poprzednich wyborów — do komunikatu „wybierz z powrotem X".
    fn state_raw(&self) -> Option<&str> {
        self.state_raw.as_deref()
    }

    /// Od tej chwili każda zmiana domyślnego wyjścia jest zmianą użytkownika.
    fn arm(&self) {
        self.armed.set(true);
    }
}

/// Enumeracja węzłów Audio/Sink + odczyt aktualnie skonfigurowanego
/// domyślnego wyjścia, w jednej sesji (podkomendy `devices` i `check`).
pub fn discover_sinks() -> Result<GraphSnapshot> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;
    let (sinks, session_manager) = enumerate_sinks(&mainloop, &core)?;
    let (defaults, _watch) = read_default_sink_name(&mainloop, &core);
    Ok(GraphSnapshot {
        sinks,
        defaults,
        session_manager,
    })
}

/// Jednorazowa migawka grafu dla podkomend `devices` i `check`.
pub struct GraphSnapshot {
    pub sinks: Vec<SinkInfo>,
    pub defaults: DefaultSinks,
    /// Nazwa klienta menedżera sesji, jeśli jest w grafie.
    ///
    /// Po co: cały model przelotki stoi na politykach Lua WirePlumbera —
    /// `filter.smart` jest tym, co w ogóle wpina nas w tor. Bez WirePlumbera
    /// (goły PipeWire, `pipewire-media-session`, własna polityka) węzeł
    /// powstanie i nikt go z niczym nie zlinkuje. Bez tego odczytu `check`
    /// drukował „OK translator wepnie się w aktualne domyślne wyjście
    /// (filter.smart)" KAŻDEMU, także temu, u kogo nie ma czego wpiąć.
    pub session_manager: Option<String>,
}

/// Plik stanu WirePlumbera z zapamiętanym wyborem wyjścia (względem $HOME).
/// TYLKO DO ODCZYTU — program nie zapisuje konfiguracji audio użytkownika.
const WP_DEFAULT_NODES_STATE: &str = ".local/state/wireplumber/default-nodes";

/// Nazwa urządzenia, które użytkownik miał wybrane, ZANIM domyślnym wyjściem
/// stał się nasz węzeł — z pliku stanu WirePlumbera.
///
/// WirePlumber trzyma tam nie jedną wartość, tylko stos: bieżący wybór pod
/// `default.configured.audio.sink`, a poprzednie pod `...sink.0`, `.1`, `.2`
/// (rosnąco = coraz starsze). Gdy bieżącym wyborem jesteśmy MY, jedyne
/// miejsce, gdzie przetrwała nazwa prawdziwego sprzętu, to właśnie ten stos —
/// metadana w PipeWire jest już nadpisana.
///
/// Bierzemy pierwszy wpis, który nie jest naszym węzłem: gdy użytkownik
/// przełączał się na nas kilka razy, `.0` też potrafi wskazywać na nas.
/// Format pliku jest INI-podobny (sekcja `[default-nodes]`, potem
/// `klucz=wartość`), więc parsujemy go po znaku `=` i nic więcej nie zakładamy.
fn previous_configured_sink(raw: &str) -> Option<String> {
    let own = [SINK_NODE_NAME, OUT_NODE_NAME];
    let mut candidates: Vec<(u32, &str)> = raw
        .lines()
        .filter_map(|l| l.split_once('='))
        .filter_map(|(k, v)| {
            let idx = k.trim().strip_prefix("default.configured.audio.sink.")?;
            Some((idx.parse::<u32>().ok()?, v.trim()))
        })
        .filter(|(_, v)| !v.is_empty() && !own.contains(v))
        .collect();
    // Kolejność linii w pliku jest przypadkowa (to zrzut tablicy haszującej),
    // więc „pierwszy" musi znaczyć „o najniższym indeksie", a nie „najwyżej".
    candidates.sort_by_key(|(i, _)| *i);
    candidates.first().map(|(_, v)| v.to_string())
}

/// Reakcja programu na sytuację „domyślnym wyjściem dźwięku jest NASZ węzeł":
/// jeden wpis w logu, osobno dla każdego z dwóch kluczy metadanych.
///
/// DLACZEGO `warn`, A NIE `error`. Wcześniejsza wersja tej funkcji krzyczała
/// `log::error!`, że translator „będzie NIEMY". To była nieprawda i trzeba to
/// powiedzieć wprost, bo na tej tezie wisiała cała instrukcja odsłuchu.
/// Gdy domyślnym sinkiem jesteśmy my, find-default-target.lua dostaje nasz
/// własny `main_si`, `canLink` odmawia — ale hook NIE robi `stop_processing`,
/// więc sterowanie idzie do linking/find-best-target.lua, ten pomija
/// inteligentne filtry (nas) i wybiera najlepszy sprzętowy sink. Nasz
/// strumień linkuje się do sprzętu, aplikacje trafiają w nas jako w domyślne
/// wyjście i dźwięk PRZECHODZI. Stan jest mylący (translator wygląda
/// w miksererach jak urządzenie, a jego nazwa zatyka stos zapamiętanych
/// wyborów), ale nie jest awarią — nie wolno więc ani nazywać go awarią,
/// ani zwracać za niego niezerowego kodu wyjścia z `check`.
///
/// DLACZEGO MIMO TO OSTRZEGAMY. Naprawić ten stan dałoby się jednym zapisem
/// `default.configured.audio.sink` — i tego WŁAŚNIE nie robimy: program nie
/// nadpisuje konfiguracji audio użytkownika. Zostaje powiedzieć, co jest nie
/// tak i co kliknąć.
///
/// Zwraca `true`, gdy nazwa wskazuje na nas (wołający pomija wtedy zwykły log
/// o przepięciu — nie było przepięcia na cudze urządzenie).
fn warn_if_default_is_us(
    slot: DefaultSlot,
    default_sink: Option<&str>,
    state_raw: Option<&str>,
) -> bool {
    let own = [SINK_NODE_NAME, OUT_NODE_NAME];
    if !default_sink.is_some_and(|n| own.contains(&n)) {
        return false;
    }
    let hint = match state_raw.and_then(previous_configured_sink) {
        Some(prev) => format!("Wybierz z powrotem swoje urządzenie, czyli \"{prev}\""),
        // Brak pliku stanu albo sam nasz węzeł w całym stosie: nie zgadujemy
        // sprzętu — od tego jest `nacelle-translator devices`.
        None => "Wybierz swoje urządzenie (lista: nacelle-translator devices)".to_string(),
    };
    // Dwa różne stany, dwa różne komunikaty. `Active` to ten, który realnie
    // wpływa na routing — i ten, który WirePlumber potrafi ustawić SAM, bez
    // udziału użytkownika (np. gdy zniknie ostatni sprzętowy sink, a nasz
    // węzeł zostanie jedynym w systemie: fallback-sink.lua liczy nas jako
    // zwykły sink, bo filtruje po `wireplumber.is-virtual`, a nie po
    // `node.virtual`, więc nawet „Dummy Output" wtedy nie powstanie).
    match slot {
        DefaultSlot::Active => log::warn!(
            "aktywnym domyślnym wyjściem dźwięku (default.audio.sink) jest NASZ własny węzeł \
             ({SINK_NODE_NAME}). Dźwięk gra dalej — WirePlumber dopina nasz strumień do \
             sprzętu przez linking/find-best-target.lua — ale to ustawienie jest mylące \
             i potrafi znaczyć, że zniknęło Twoje urządzenie wyjściowe. Translator jest \
             przelotką w torze, a nie urządzeniem: nie wybiera się go jako wyjścia, tylko \
             wpina się sam w to, co masz wybrane. {hint} — w Ustawieniach systemowych KDE → \
             Dźwięk (albo `wpctl set-default <ID>`). Translator podąży za tym wyborem sam, \
             bez restartu."
        ),
        DefaultSlot::Configured => log::warn!(
            "zapamiętanym wyborem wyjścia dźwięku (default.configured.audio.sink) jest NASZ \
             własny węzeł ({SINK_NODE_NAME}). To nie zatrzymuje dźwięku, ale zatyka stos \
             zapamiętanych wyborów WirePlumbera i po każdym restarcie wraca. {hint} — \
             w Ustawieniach systemowych KDE → Dźwięk."
        ),
    }
    true
}

/// Odczyt pliku stanu WirePlumbera. `None` przy braku pliku albo braku $HOME —
/// to nie jest błąd, tylko brak podpowiedzi w komunikacie.
fn read_wp_state() -> Option<String> {
    let home = std::env::var_os("HOME")?;
    std::fs::read_to_string(std::path::Path::new(&home).join(WP_DEFAULT_NODES_STATE)).ok()
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
    fatal: &Fatal,
    ml: &pw::main_loop::MainLoopRc,
) -> impl FnMut(&pw::stream::Stream, &mut D, StreamState, StreamState) {
    let fatal = fatal.clone();
    let ml = ml.clone();
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
                    // W modelu przelotki chwilowy brak celu jest ścieżką
                    // NORMALNĄ (przełączanie urządzenia w aplecie, zmiana
                    // profilu karty, uśpione słuchawki) — WirePlumber
                    // dolinkuje nas z powrotem sam.
                    log::warn!(
                        "{name}: błąd sesyjny od serwera (zwykle przejściowy brak celu albo \
                         nieudany link), stan strumienia bez zmian — pracuję dalej: \
                         {msg}{tail}"
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
                    StreamState::Error(m) => format!("{name}: błąd strumienia: {m}"),
                    // To NIE jest „zniknął cel" — utrata celu jest tu ścieżką
                    // normalną i kończy się warnem wyżej. `Unconnected`
                    // powstaje wyłącznie w proxy_removed/proxy_destroy/
                    // on_core_error(-EPIPE), czyli gdy serwer zniszczył nasz
                    // węzeł albo padło połączenie.
                    _ => format!(
                        "{name}: węzeł wypadł z grafu — serwer PipeWire zniszczył nasz węzeł \
                         albo zerwało się połączenie z demonem.{hint} \
                         Co zrobić: sprawdź `systemctl --user status pipewire wireplumber` \
                         i uruchom translator ponownie."
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

    // Odczyt (i TYLKO odczyt) domyślnego wyjścia. Nie po to, żeby coś wybrać
    // — wybiera WirePlumber — tylko żeby odnotować stan, w którym domyślnym
    // wyjściem jesteśmy my sami.
    // `default_watch` MUSI dożyć końca `run_graph`: jego wcześniejszy drop
    // wypina słuchacza i późniejsze zmiany urządzenia znikają z logu.
    let (default_sink, default_watch) = read_default_sink_name(&mainloop, &core);
    // OBA sloty osobno: `configured` bywa nieaktualny (wskazuje sprzęt, którego
    // nie ma), a o routingu decyduje `active`. Sklejenie ich przez `or_else`
    // potrafiło pokazać sprzęt i przemilczeć, że aktywnym wyjściem jesteśmy MY.
    warn_if_default_is_us(
        DefaultSlot::Active,
        default_sink.active.as_deref(),
        default_watch.state_raw(),
    );
    warn_if_default_is_us(
        DefaultSlot::Configured,
        default_sink.configured.as_deref(),
        default_watch.state_raw(),
    );
    match (
        default_sink.active.as_deref(),
        default_sink.configured.as_deref(),
    ) {
        (Some(a), Some(c)) if a != c => log::info!(
            "domyślne wyjście (odczyt): aktywne \"{a}\", zapamiętany wybór \"{c}\" — liczy się \
             aktywne, to po nie sięga WirePlumber przy wyborze celu"
        ),
        (Some(a), _) => log::info!("aktualne domyślne wyjście (odczyt): {a}"),
        (None, Some(c)) => log::info!("zapamiętany wybór wyjścia (odczyt): {c}"),
        (None, None) => log::info!(
            "nie udało się odczytać domyślnego wyjścia — to nie jest błąd, cel i tak wybiera \
             WirePlumber"
        ),
    }
    // Od tej chwili każde wywołanie callbacku metadanych to zmiana zrobiona
    // przez użytkownika, a nie zrzut startowy.
    default_watch.arm();

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
            // ZAMIERZONE, nie obejście: przelotki nie wybiera się jako
            // urządzenia. `node.virtual=true` + `filterVirtualDevices`
            // w plasma-pa chowa nas z listy wyjść w aplecie, czyli dokładnie
            // to, czego chcemy — użytkownik ma widzieć swój sprzęt, a nas nie.
            // Wcześniej ta właściwość była problemem, bo trzeba było nas
            // wskazać ręcznie; od `filter.smart` wpinamy się sami.
            "node.virtual" => "true",
            "node.link-group" => LINK_GROUP,
            // Bez tego KLUCZA cały mechanizm inteligentnych filtrów śpi:
            // getFilterSmart (filter-utils.lua:36-37) zwraca false i nikt
            // nigdy nie wpina nas w tor. To jest jedyna właściwość, która
            // odróżnia „para węzłów w jednej grupie" od „przelotka".
            "filter.smart" => "true",
            "filter.smart.name" => FILTER_SMART_NAME,
            // CELOWO BRAK `filter.smart.target` — patrz nagłówek pliku.
            // Filtr bezcelowy wpina się w AKTUALNIE domyślne wyjście
            // (wzorzec EasyEffects); podanie tego klucza przypięłoby nas
            // do jednego sprzętu na sztywno.
            //
            // Nie usypiaj nas po bezczynności: suspend-node.lua:39-45 przy
            // wartości 0 wychodzi bez ustawiania timera. Zawieszony węzeł
            // filtra znika z toru, a jego powrót to dodatkowa dziura
            // w dźwięku przy pierwszym odtwarzaniu.
            "session.suspend-timeout-seconds" => "0",
            // Nie przywracaj nam zapamiętanej głośności (state-stream.lua:58
            // — nasz Audio/Sink bez `device.routes` też tam wpada). Nasz
            // węzeł stoi w torze CAŁEGO dźwięku, więc stara wartość z pliku
            // stanu ściszyłaby cichcem cały system, w miejscu, którego
            // użytkownik nie widzi nawet w aplecie.
            "state.restore-props" => "false",
            // Symetrycznie do STREAM-u (patrz niżej): hook `node/restore-stream`
            // łapie też `Audio/*` bez `device.routes`, czyli nas, i przy
            // zapamiętanym celu wstawiłby nam `target.object` do metadanych.
            "state.restore-target" => "false",
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
                // downmix mono dla toru AI; pełny ring => nadmiar przepada.
                // Przy zamkniętej bramce pomijamy CAŁOŚĆ (także sam downmix):
                // przelotka ma wtedy kosztować tyle co przepisanie próbek,
                // a nie tyle co przepisanie plus pętla mnożeń na darmo.
                let lost = feed_ai(
                    st.gate,
                    &st.stereo[..nsamples],
                    &mut st.mono,
                    &mut st.cap_prod,
                );
                if lost > 0 {
                    st.cap_dropped.fetch_add(lost as u64, Ordering::Relaxed);
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

    // ---------- 2) odtwarzanie (Direction::Output) — cel wybiera WirePlumber ----------
    let mut play_props = properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Playback",
        *pw::keys::NODE_NAME => OUT_NODE_NAME,
        "node.link-group" => LINK_GROUP,
        // NIE POZWÓL WirePlumBEROWI PRZYWRÓCIĆ NAM CELU. Bez tego klucza
        // node/state-stream.lua (hook `node/restore-stream`, interest
        // `media.class matches "Stream/*"` — czyli MY) przy każdym starcie
        // czyta zapamiętany cel i robi `metadata:set(bound-id,
        // "target.object", ...)`. Ten wpis czyta potem
        // find-defined-target.lua i ustawia `has_defined_target = true`,
        // czyli DOKŁADNIE to przypięcie do jednego sprzętu, które ten model
        // usuwa — tylko tylnymi drzwiami i trwale, bez śladu w naszym logu.
        // Wystarczy, że użytkownik RAZ przeciągnie „nacelle-translator-out"
        // na urządzenie w pavucontrol (nasz STREAM nie ma `node.virtual`,
        // więc jest tam widoczny). Gałąź pomijająca (`target_in_props`)
        // sprawdza WŁAŚCIWOŚCI węzła, a my ich celowo nie ustawiamy, więc
        // nas nie chroni. Klucz blokuje obie strony: zapis
        // (state-stream.lua:228) i odtworzenie (state-stream.lua:89).
        "state.restore-target" => "false",
        // Ta sama furtka na głośność/wyciszenie, zamknięta już na MAIN.
        // W ~/.local/state/wireplumber/stream-properties na tej maszynie JUŻ
        // istnieje wpis `Output/Audio:media.name:nacelle-translator-out` —
        // jedno wyciszenie tego strumienia w mikserze i `node:set_param
        // ("Props")` przywracałoby mute przy każdym starcie, dając niemą
        // przelotkę bez ani jednego komunikatu. Wzorzec valve-galileo
        // (wireplumber.conf.d/stream.conf) ustawia ten klucz dla OBU węzłów
        // pary, nie tylko dla sinka — komentarz w źródle mówi wprost
        // „in case the user has somehow managed to mute them".
        "state.restore-props" => "false",
        // CELOWO BRAK `target.object` i `node.dont-fallback` — patrz nagłówek
        // pliku, sekcja „CZEGO TU CELOWO NIE MA". Pierwsze przypięłoby nas
        // do jednego sprzętu, drugie kazałoby WirePlumberowi niszczyć nasz
        // węzeł przy każdym rescanie (find-filter-target.lua, gałąź
        // `is_smart_filter and dont_fallback`).
    };
    // `node.passive` TYLKO przy zamkniętej bramce — i to nie jest oszczędzanie
    // na siłę, tylko warunek poprawności.
    //
    // Pasywny węzeł nie trzyma sinka zajętym: „if the node is not otherwise
    // linked (via a non-passive link), the node and the sink it is linked to
    // are idle (and eventually suspended)" (man 7 pipewire-props). Dla czystej
    // przelotki jest to w 100% poprawne — nie mamy nic własnego do zagrania,
    // więc gdy źródło milczy, sprzęt ma prawo zasnąć razem z nami. Wzorzec
    // valve-galileo ustawia `node.passive` właśnie na filtrze bez własnego
    // źródła dźwięku.
    //
    // Przy WŁĄCZONYM tłumaczeniu nie jesteśmy jednak wyłącznie filtrem: jesteśmy
    // też NIEZALEŻNYM ŹRÓDŁEM (lektor). Scenariusz: film kończy się zdaniem,
    // VAD zamyka segment, whisper + tłumaczenie + piper mielą kilka sekund,
    // a w tym czasie źródło przestaje grać i pasywna grupa idzie w idle. Wtedy
    // callback odtwarzania przestaje być wołany i zsyntetyzowane zdanie NIE MA
    // CZYM się odegrać: albo zostaje w ringu i odzywa się przy zupełnie innym
    // materiale, albo `tts_thread` blokuje się na pełnym ringu i po 10 s
    // porzuca ogon wypowiedzi (`stall_decision`). Węzeł, który sam produkuje
    // dźwięk, musi umieć obudzić wyjście — czyli nie może być pasywny.
    if !gate.feeds_ai() {
        play_props.insert("node.passive", "true");
    }
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

                    mix_chunk(
                        st.gate,
                        &st.duck,
                        &mut st.gain,
                        &mut st.hold,
                        &st.pass_scratch[..got_pass_frames * CHANNELS],
                        &st.tts_scratch[..got_tts],
                        &mut out[fi * CHANNELS..(fi + n) * CHANNELS],
                    );
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
        // BEZ `DONT_RECONNECT` — to jest właśnie ta flaga, która czyniła
        // przelotkę niemożliwą: prepare-link.lua:73-76 po pierwszym
        // zlinkowaniu zerowałby cel zamiast przenieść nas na nowe wyjście.
        // „Reconnect" to w modelu przelotki ścieżka NORMALNA, nie wyjątek.
        StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
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
                    // dotyka: node/suspend-node.lua filtruje `Audio/*`,
                    // a my mamy `Stream/Output/Audio`. Zerowa liczba cykli
                    // znaczy zawieszony węzeł albo stojący serwer.
                    //
                    // Przy ZAMKNIĘTEJ bramce `node.passive=true` DOPUSZCZA,
                    // żeby ta linia pojawiała się w normalnej pracy: gdy nikt
                    // nic nie gra, sprzęt zasypia razem z nami i o to chodzi.
                    // Przy otwartej bramce nie jesteśmy pasywni (patrz
                    // `play_props`), więc tam ta linia to już realny sygnał.
                    None => log::debug!(
                        "passthrough: ani jednego cyklu odtwarzania w ostatnich {}s \
                         — węzeł zawieszony albo serwer stanął",
                        DROP_REPORT_INTERVAL.as_secs()
                    ),
                }
            })?;
    }

    log::info!(
        "przelotka \"Nacelle Translator (PL)\" gotowa (node.name={SINK_NODE_NAME}, \
         filter.smart) — NIC NIE TRZEBA USTAWIAĆ. Węzeł wpina się sam w to wyjście, które \
         masz aktualnie wybrane w KDE, i podąża za każdą jego zmianą bez restartu. \
         W aplecie głośności go NIE ZOBACZYSZ i tak ma być: to przelotka w torze, \
         a nie urządzenie do wybrania. Ctrl+C kończy"
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

    /// Realny kształt pliku ~/.local/state/wireplumber/default-nodes w chwili,
    /// gdy użytkownik ma ustawiony NASZ węzeł jako domyślne wyjście.
    const STATE: &str = "\
[default-nodes]
default.configured.audio.sink=nacelle-translator-sink
default.configured.audio.sink.0=alsa_output.usb-Logitech.analog-stereo
default.configured.audio.sink.1=alsa_output.pci-0000_73_00.6.analog-stereo
";

    #[test]
    fn p1_stos_wyborow_daje_poprzednie_urzadzenie() {
        assert_eq!(
            previous_configured_sink(STATE).as_deref(),
            Some("alsa_output.usb-Logitech.analog-stereo")
        );
    }

    #[test]
    fn p2_najnizszy_indeks_wygrywa_niezaleznie_od_kolejnosci_linii() {
        // Plik jest zrzutem tablicy haszującej — kolejność linii bywa dowolna,
        // więc „pierwszy" musi wynikać z indeksu, a nie z pozycji w pliku.
        let raw = "default.configured.audio.sink.2=c\n\
                   default.configured.audio.sink.0=a\n\
                   default.configured.audio.sink.1=b\n";
        assert_eq!(previous_configured_sink(raw).as_deref(), Some("a"));
    }

    #[test]
    fn p3_wlasne_wezly_nie_moga_byc_podpowiedzia() {
        // Po kilku przełączeniach na nas potrafimy siedzieć także w stosie;
        // podpowiedź „wybierz z powrotem translator" byłaby błędnym kołem.
        let raw = format!(
            "default.configured.audio.sink.0={SINK_NODE_NAME}\n\
             default.configured.audio.sink.1={OUT_NODE_NAME}\n\
             default.configured.audio.sink.2=alsa_output.realny\n"
        );
        assert_eq!(
            previous_configured_sink(&raw).as_deref(),
            Some("alsa_output.realny")
        );
        // sam nasz węzeł w całym stosie = brak podpowiedzi, NIE zgadywanka
        let tylko_my = format!("default.configured.audio.sink.0={SINK_NODE_NAME}\n");
        assert_eq!(previous_configured_sink(&tylko_my), None);
    }

    #[test]
    fn p4_smieci_i_klucz_biezacy_nie_wchodza_do_stosu() {
        // `default.configured.audio.sink` BEZ indeksu to wybór BIEŻĄCY (czyli
        // my) — nie wolno go zwrócić jako podpowiedzi
        let raw = "[default-nodes]\n\
                   default.configured.audio.sink=nacelle-translator-sink\n\
                   default.configured.audio.source.0=alsa_input.mikrofon\n\
                   default.configured.audio.sink.x=nie-liczba\n\
                   default.configured.audio.sink.0=\n";
        assert_eq!(previous_configured_sink(raw), None);
        assert_eq!(previous_configured_sink(""), None);
    }

    #[test]
    fn p5_alarm_tylko_gdy_domyslnym_jestesmy_my() {
        use DefaultSlot::*;
        // Zwykły sprzęt jako domyślne wyjście to stan NORMALNY — ani słowa.
        assert!(!warn_if_default_is_us(
            Active,
            Some("alsa_output.cokolwiek"),
            Some(STATE)
        ));
        // Brak odczytu też nie jest powodem do alarmu: cel i tak wybiera
        // WirePlumber, a my o tym nie decydujemy.
        assert!(!warn_if_default_is_us(Active, None, Some(STATE)));
        // Oba nasze węzły są powodem, w obu slotach.
        assert!(warn_if_default_is_us(Active, Some(SINK_NODE_NAME), Some(STATE)));
        assert!(warn_if_default_is_us(Active, Some(OUT_NODE_NAME), Some(STATE)));
        assert!(warn_if_default_is_us(
            Configured,
            Some(SINK_NODE_NAME),
            Some(STATE)
        ));
        // ... i brak pliku stanu nie może tego alarmu wyciszyć
        assert!(warn_if_default_is_us(Active, Some(SINK_NODE_NAME), None));
    }

    #[test]
    fn p6_aktywne_wyjscie_nie_moze_sie_schowac_za_zapamietanym() {
        // TA klasa błędu przeszła recenzję poprzedniej wersji: kod sklejał oba
        // sloty przez `configured.or_else(active)`, więc stan „zapamiętany
        // wybór wskazuje sprzęt, a AKTYWNYM wyjściem jesteśmy MY" zwracał
        // nazwę sprzętu i nikt się o nim nie dowiadywał. A to właśnie ten
        // slot decyduje o wyborze celu (findDefaultLinkable → get-default-node).
        let d = DefaultSinks {
            configured: Some("alsa_output.cokolwiek".into()),
            active: Some(SINK_NODE_NAME.into()),
        };
        assert_eq!(d.effective(), Some(SINK_NODE_NAME));
        assert!(warn_if_default_is_us(
            DefaultSlot::Active,
            d.active.as_deref(),
            Some(STATE)
        ));
        assert!(!warn_if_default_is_us(
            DefaultSlot::Configured,
            d.configured.as_deref(),
            Some(STATE)
        ));
    }

    #[test]
    fn p7_zapamietany_wybor_nie_udaje_aktywnego() {
        // Odwrotny rozjazd, obserwowany na żywo: `default.configured` wskazuje
        // nasz węzeł (zostałość po starym modelu), a aktywnym wyjściem jest
        // prawdziwy sprzęt, bo naszego węzła akurat nie ma w grafie.
        // `effective()` musi pokazać sprzęt — inaczej `devices` gubi gwiazdkę,
        // a `check` opisuje stan, którego nie ma.
        let d = DefaultSinks {
            configured: Some(SINK_NODE_NAME.into()),
            active: Some("alsa_output.cokolwiek".into()),
        };
        assert_eq!(d.effective(), Some("alsa_output.cokolwiek"));
        // brak aktywnego = spadamy na zapamiętany, bo lepsza taka nazwa niż żadna
        let tylko_configured = DefaultSinks {
            configured: Some("alsa_output.x".into()),
            active: None,
        };
        assert_eq!(tylko_configured.effective(), Some("alsa_output.x"));
        assert_eq!(DefaultSinks::default().effective(), None);
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

    /// Parametry duckingu do testów: `pass_gain` != 1.0 i `duck_gain` daleko
    /// od niego, żeby pomylenie jednego z drugim nie przeszło niezauważone,
    /// a współczynniki 1.0 (natychmiastowe dojście do celu), żeby test nie
    /// mierzył kształtu obwiedni, tylko to, czy w ogóle się odpaliła.
    fn duck_testowy() -> DuckParams {
        DuckParams {
            pass_gain: 0.5,
            duck_gain: 0.1,
            tts_gain: 1.0,
            attack_coef: 1.0,
            release_coef: 1.0,
            hold_frames: 3,
        }
    }

    #[test]
    fn b3_zamknieta_bramka_nie_odpala_duckingu_mimo_probek_w_ringu() {
        // TO jest twierdzenie, które commit bramkowy postawił, a którego nie
        // pilnował ŻADEN test: „got_tts = 0, więc ducking się nie odpala".
        // Poprzednie b3 porównywało dwie metody czytające to samo pole jednej
        // struktury — przechodziło z definicji i nie mogło wykryć klasy błędu,
        // dla której powstało (pomyłka w MIEJSCU UŻYCIA bramki).
        //
        // Tu podajemy niezerowego lektora przy ZAMKNIĘTEJ bramce. Gdyby
        // ducking się odpalił, ścisząłby CAŁY dźwięk systemu o ~14 dB.
        let duck = duck_testowy();
        let mut gain = duck.pass_gain;
        let mut hold = 0u32;
        let pass = [0.2, -0.2, 0.4, -0.4];
        let tts = [0.9, 0.9];
        let mut out = [0.0f32; 4];
        mix_chunk(
            TranslateGate::new(false),
            &duck,
            &mut gain,
            &mut hold,
            &pass,
            &tts,
            &mut out,
        );
        // wyjście = sam oryginał przemnożony przez pass_gain, bez śladu lektora
        for (i, o) in out.iter().enumerate() {
            assert!(
                (o - pass[i] * duck.pass_gain).abs() < 1e-6,
                "zamknięta bramka zmieniła próbkę {i}: {o}"
            );
        }
        assert_eq!(gain, duck.pass_gain, "wzmocnienie nie miało prawa drgnąć");
        assert_eq!(hold, 0, "zatrzymanie duckingu nie miało prawa się uzbroić");
    }

    #[test]
    fn b3b_otwarta_bramka_duckuje_i_dodaje_lektora() {
        // Druga strona tego samego niezmiennika: sprawdzamy, że przy otwartej
        // bramce nic nie zostało zepsute po drodze. Bez tego test wyżej dałoby
        // się „naprawić" wyłączając ducking na stałe.
        let duck = duck_testowy();
        let mut gain = duck.pass_gain;
        let mut hold = 0u32;
        let pass = [0.2, -0.2, 0.4, -0.4];
        let tts = [0.3, 0.3];
        let mut out = [0.0f32; 4];
        mix_chunk(
            TranslateGate::new(true),
            &duck,
            &mut gain,
            &mut hold,
            &pass,
            &tts,
            &mut out,
        );
        assert_eq!(gain, duck.duck_gain, "przy graniu lektora ma być duck_gain");
        assert_eq!(hold, duck.hold_frames, "zatrzask przyciszenia ma być uzbrojony");
        // oryginał przyciszony + lektor dodany
        assert!((out[0] - (0.2 * duck.duck_gain + 0.3)).abs() < 1e-6, "{}", out[0]);
    }

    #[test]
    fn b3c_underrun_passthrough_to_cisza_nie_smieci() {
        // Gdy ring passthrough nie nadążył, brakujące ramki mają być ciszą,
        // a nie resztką poprzedniego kawałka ze scratchu.
        let duck = duck_testowy();
        let mut gain = duck.pass_gain;
        let mut hold = 0u32;
        let mut out = [7.0f32; 6]; // celowo śmieci na wejściu
        mix_chunk(
            TranslateGate::new(false),
            &duck,
            &mut gain,
            &mut hold,
            &[0.2, 0.2], // tylko jedna ramka oryginału na trzy żądane
            &[],
            &mut out,
        );
        assert!((out[0] - 0.2 * duck.pass_gain).abs() < 1e-6);
        assert_eq!(&out[2..], &[0.0; 4], "brakujące ramki muszą być ciszą");
    }

    #[test]
    fn b3d_zamknieta_bramka_nie_rusza_downmiksu_ani_ringu() {
        // Commit bramkowy deklaruje, że przy zamkniętej bramce pomijamy
        // CAŁOŚĆ — także sam downmiks, nie tylko `push_slice`. Bez tego testu
        // to zdanie nie było niczym poparte.
        let (mut prod, cons) = ringbuf::HeapRb::<f32>::new(16).split();
        let stereo = [1.0, 1.0, 1.0, 1.0];
        let mut mono = [-1.0f32; 2];
        let lost = feed_ai(TranslateGate::new(false), &stereo, &mut mono, &mut prod);
        assert_eq!(lost, 0);
        assert_eq!(mono, [-1.0, -1.0], "downmiks nie miał prawa się wykonać");
        assert_eq!(cons.occupied_len(), 0, "ring toru AI ma zostać pusty");
    }

    #[test]
    fn b3e_otwarta_bramka_downmiksuje_i_liczy_zgubione() {
        let (mut prod, cons) = ringbuf::HeapRb::<f32>::new(2).split();
        // 3 ramki stereo, w ringu miejsce na 2 próbki mono => jedna przepada
        let stereo = [1.0, 0.0, 0.5, 0.5, 0.2, 0.8];
        let mut mono = [0.0f32; 3];
        let lost = feed_ai(TranslateGate::new(true), &stereo, &mut mono, &mut prod);
        assert_eq!(mono, [0.5, 0.5, 0.5], "downmiks to średnia obu kanałów");
        assert_eq!(cons.occupied_len(), 2);
        assert_eq!(lost, 1, "nadmiar musi trafić do licznika porzuceń");
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
