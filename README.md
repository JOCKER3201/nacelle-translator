# nacelle-translator

Węzeł PipeWire, który **tłumaczy w locie dźwięk w drodze do urządzenia
wyjściowego**.

Model działania to **przelotowy łącznik z tłumaczem w środku**: urządzenie
wyjściowe wybierasz w KDE tak jak zawsze, a translator wpina się w ten tor
sam i podąża za każdą zmianą urządzenia — bez restartu, bez ustawiania go
jako wyjścia, bez jednego kliknięcia więcej. Technicznie robi to jako
*inteligentny filtr* WirePlumbera (`filter.smart`), dokładnie tym samym
mechanizmem co EasyEffects. W aplecie głośności **nie zobaczysz** żadnego
nowego urządzenia i tak ma być.

Dźwięk przechodzący przez przelotkę jest — o ile włączysz tłumaczenie —
segmentowany, transkrybowany, tłumaczony na polski i czytany głosem lektora,
a oryginał przechodzi dalej przyciszany pod lektorem (ducking, jak
w telewizyjnym szeptance).

> **Uwaga o autorstwie:** ten projekt został w całości zaprojektowany
> i napisany przez **Claude (Anthropic)** — model Claude Fable 5 działający
> w Claude Code — na zlecenie właściciela repozytorium.

## Architektura

```
aplikacje ──▶ [Nacelle Translator (PL)]  (smart filter, media.class=Audio/Sink)
                    │  ▲ WirePlumber wpina nas tu sam, bo filtr jest
                    │  │ „bezcelowy" (filter.smart bez filter.smart.target)
                    │ RT: ringbuffery SPSC (zero blokad w wątku danych)
                    ├─ passthrough stereo ────────────────────┐
                    └─ downmix mono 48 kHz  [audio].translate │
                         └─▶ rubato 48k→16k ─▶ Silero VAD     │ ducking
                              └─▶ segmenter (histereza,       │ (-14 dB pod
                                   hangover 800 ms,           │  lektorem)
                                   cięcie w dołku p.)         ▼
                                   └─▶ whisper.cpp ─▶ llama-server ─▶ piper
                                     (STT, CUDA)    (tłumaczenie)   (głos pl)
                                                                    │
    AKTUALNE domyślne wyjście ◀── [nacelle-translator-out] ◀─ mikser ┘
    (cel wybiera WirePlumber,       (strumień Playback, node.passive)
     przepina bez restartu)
```

Gałąź AI (od `rubato` w dół) rusza wyłącznie przy `[audio].translate = true`;
przy `false` przez węzeł idzie sam passthrough.

- **STT:** [whisper.cpp] przez `whisper-rs` (backend CUDA — feature `cuda`,
  model wielojęzyczny, autodetekcja języka źródłowego). Wymaga CUDA Toolkit
  w środowisku budowania (patrz „Budowanie z CUDA" niżej) — na hoście
  wystarczy sam sterownik NVIDIA.
- **MT:** zalecany silnik to **`engine = "llamacpp"`** (ustaw w
  `nacelle-translator.toml`) — lokalny `llama-server` na GPU serwujący model
  **TranslateGemma-4B-IT** (GGUF, dedykowany model tłumaczeniowy Google, nie
  ogólny czat), wybrany bo mieści się w kilku GB VRAM i zostawia miejsce na
  jednoczesne granie na tym samym GPU. Uwaga: domyślną wartością w kodzie
  (gdy nie ma pliku konfiguracyjnego) jest `"gemini"` — API w chmurze
  (klucz w `GEMINI_API_KEY`), które działa bez lokalnego serwera. Pozostałe
  silniki: **Ollama** (`engine = "ollama"`, `http://localhost:11434`, model w
  `ollama_model` — lokalnie na CPU, zbyt wolne na dużych wagach), **API
  Claude** (`engine = "claude"`, Messages API, klucz w `ANTHROPIC_API_KEY`)
  albo `"off"` do testu samego toru audio.
- **TTS:** [piper] jako jeden długożyjący proces (`--json-input`, WAV na
  tmpfs) z głosem `pl_PL-gosia-medium` (ścieżki w `[tts] piper_bin`
  i `[tts] voice`; skąd wziąć — patrz „Wymagania").
- **VAD:** Silero V5 (`voice_activity_detector`) — VAD energetyczny odpada,
  bo muzyka pod mową trzyma bramkę otwartą.
- Wypowiedzi już po polsku są rozpoznawane i **pomijane** (przechodzi sam
  oryginał, bez duckingu).

Opóźnienie z natury rzeczy: lektor odzywa się kilka sekund po oryginale
(segmentacja per wypowiedź + STT na GPU + tłumaczenie + synteza; przy
lokalnym modelu Ollamy pierwsza wypowiedź po dłuższej ciszy płaci dodatkowo
czas ładowania modelu do pamięci — `nacelle-translator` rozgrzewa go na starcie,
żeby zminimalizować ten koszt w trakcie sesji). Oryginał gra na bieżąco,
więc obraz się nie rozjeżdża — spóźnia się tylko głos lektora.

## Wymagania

- **PipeWire** (daemon) + nagłówki `libpipewire-0.3`, **cmake**, **clang /
  libclang**, **Rust**. Na typowej dystrybucji instalujesz je z pakietów:
  - Fedora: `pipewire-devel clang-devel cmake` (+ `rustup`),
  - Debian/Ubuntu: `libpipewire-0.3-dev libclang-dev cmake`,
  - Arch: `pipewire clang cmake rust`.

  Autor pracuje na Bazzite (rpm-ostree), gdzie nagłówków nie da się
  doinstalować systemowo — stąd w `build.sh` ścieżka przez **Homebrew**
  (`brew install pipewire`, wersja zgodna z systemową 1.6.8). Na zwykłej
  dystrybucji Homebrew nie jest potrzebne; `build.sh` używa go tylko, jeśli
  wykryje `HOMEBREW_PREFIX`.
- **piper** + polski głos `pl_PL-gosia-medium` w `~/.local/share/piper`
  (binarka piper ma RUNPATH=$ORIGIN, żadnych zmiennych środowiskowych nie
  trzeba). Binarka: [rhasspy/piper — Releases]; głos:
  [rhasspy/piper-voices] (`pl/pl_PL/gosia/medium`, pliki `.onnx`
  i `.onnx.json` obok siebie). Ścieżki zmienisz w `[tts] piper_bin`
  i `[tts] voice`.
- zalecany silnik `"llamacpp"` wymaga działającego `llama-server`
  (`llamacpp_host`, domyślnie `http://localhost:8080`) — patrz „Budowanie z
  CUDA" niżej; silnik `"gemini"` (domyślny w kodzie, gdy brak pliku
  konfiguracyjnego) wymaga `export GEMINI_API_KEY=...` (Google AI Studio);
  silnik `"ollama"` wymaga działającej usługi (`systemctl status ollama`)
  z modelem wskazanym w `ollama_model` (domyślnie `gemma`); silnik
  `"claude"` wymaga `export ANTHROPIC_API_KEY=...` — `nacelle-translator
  check` sprawdza dokładnie ten silnik, który masz ustawiony w
  `[translate] engine`,
- model whispera (jednorazowo, ~488 MB):

```sh
curl -L --create-dirs -o models/ggml-small.bin \
  https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small.bin
```

`ggml-small` to rozsądny kompromis jakości do czasu na GPU. Szybciej
(kosztem jakości): `ggml-base.bin`. **Nie używaj modeli `*.en`** — są angielsko-tylko i nie
wykrywają języka.

## Budowanie

```sh
./build.sh          # czysty build; env (PKG_CONFIG_PATH, LIBCLANG_PATH, CC/CXX) ustawia skrypt
./build.sh --fast   # bez cargo clean
```

Podczas builda crate `ort` (Silero VAD) pobiera z sieci prebuilt onnxruntime
(~50 MB), a `whisper-rs-sys` kompiluje whisper.cpp przez cmake — pierwszy
build trwa kilka minut.

## Budowanie z CUDA (feature `cuda`, domyślnie włączona)

`whisper-rs` buduje się z CUDA Toolkit (nie tylko sterownikiem NVIDIA) —
Bazzite/rpm-ostree tego nie oferuje jako pakietu Homebrew, więc build robi
się w kontenerze **distrobox** z pełnym Toolkitem (tu: kontener `Fedora`):

```sh
distrobox enter Fedora -- ./build.sh
distrobox enter Fedora -- ./package.sh   # zbiera libcudart/libcublas/libcublasLt do dist/lib
```

Gotowa binarka na hoście (sam sterownik, bez Toolkita) potrzebuje tych
trzech bibliotek obok siebie — dlatego `package.sh` je zbiera, a
`.cargo/config.toml` wpina do binarki `RPATH=$ORIGIN/../lib`, żeby je
znalazła bez `LD_LIBRARY_PATH` i bez instalowania czegokolwiek na hoście.
`libcuda.so` (sterownik) i tak zawsze musi pochodzić z hosta — to jedyna
biblioteka, której się celowo NIE dołącza.

```sh
make install          # JEDNO polecenie: czysty build + pakowanie + instalacja
                      # do /opt/nacelle-translator (bin/, lib/, models/, config);
                      # usuwa ./target przed i po pakowaniu
sudo make uninstall
```

**Bez `sudo`.** Reguła `install` sama buduje (`./build.sh --fast`), pakuje
(`./package.sh`) i dopiero kopiowanie do `/opt` woła `sudo` wewnętrznie —
dostaniesz jedno pytanie o hasło w trakcie. `sudo make install` zrobiłoby
cargo/build.sh jako root: pomyliłby właściciela plików w `./target` i mógłby
nie widzieć CUDA Toolkit spoza `PATH` roota. `make install` uruchamiaj
wewnątrz distroboxa z Toolkitem (patrz wyżej). `uninstall` to samo
`sudo rm -rf`, więc tam `sudo` zostaje.

### Silnik `llamacpp` — llama-server + TranslateGemma-4B-IT

Zalecany silnik tłumaczenia (`engine = "llamacpp"` — ustawiany ręcznie
w pliku konfiguracyjnym; domyślną wartością w kodzie jest `"gemini"`)
potrzebuje osobno uruchomionego `llama-server` (część projektu [llama.cpp], budowanego też z
CUDA Toolkit — ta sama uwaga o distroboxie co wyżej) z wczytanym modelem:

```sh
# w distroboxie "Fedora", jednorazowo:
cmake -B build -DGGML_CUDA=ON && cmake --build build --config Release -t llama-server

# uruchomienie serwera (osobny, długożyjący proces obok nacelle-translator):
./build/bin/llama-server -m translategemma-4b-it-Q5_K_M.gguf --port 8080 --no-jinja
```

`--no-jinja` jest wymagane: wbudowany szablon czatu TranslateGemma używa
strukturalnego formatu wejścia, którego generator parserów llama-server nie
umie przetworzyć (serwer bez tej flagi kończy się błędem przy starcie).
Prompt w formacie szablonu składa sam nacelle-translator (silnik `llamacpp`
odtwarza szablon z metadanych GGUF i używa surowego endpointu
`/completion`), więc jakość nie cierpi — kody języków źródłowych pochodzą
z detekcji whispera per wypowiedź.

Model (GGUF, dedykowany do tłumaczenia, nie ogólny czat) pobierz np. z
`huggingface.co/bullerwins/translategemma-4b-it-GGUF`. Licencja modelu to
[warunki Gemma](https://ai.google.dev/gemma/terms) (użycie komercyjne i
prywatne dozwolone, z załączoną Prohibited Use Policy) — nie czysty
Apache/MIT.

## Uruchomienie

```sh
./target/release/nacelle-translator check     # sprawdza model, pipera, Ollamę/klucz, PipeWire
./target/release/nacelle-translator devices   # lista węzłów Audio/Sink (podgląd grafu)
./target/release/nacelle-translator           # start; Ctrl+C kończy
```

**Nic więcej nie trzeba ustawiać.** Translator wpina się sam w to wyjście
dźwięku, które masz aktualnie wybrane w KDE, i podąża za jego zmianami.
Nie wybieraj go jako urządzenia wyjściowego — patrz „Urządzenie wyjściowe"
niżej.

Samo tłumaczenie jest domyślnie **wyłączone** (przelotka przepuszcza dźwięk
bez zmian). Włącza je `translate = true` w sekcji `[audio]` — patrz
„Tłumaczenie" niżej.

Konfiguracja: skopiuj `nacelle-translator.toml.example` do `nacelle-translator.toml` (albo
uruchamiaj bez pliku — obowiązują te same wartości domyślne). Tryb testowy
toru audio bez API: `engine = "off"` w sekcji `[translate]` (lektor czyta
oryginalny, nieprzetłumaczony tekst).

## Opcje eksperymentalne (`--experimental-features`)

Funkcje jeszcze niedomyślne włącza się WYŁĄCZNIE flagą wiersza poleceń —
nie mają kluczy w pliku konfiguracyjnym. Nazwy oddziela się **przecinkami**:

```sh
nacelle-translator --experimental-features=speculative-stt
nacelle-translator --experimental-features=speculative-stt,kolejna-opcja   # kilka naraz
nacelle-translator --experimental-features speculative-stt                 # też działa
```

Powtórzenie flagi sumuje opcje; nieznana nazwa kończy program kodem 2 i
wypisuje listę dostępnych opcji (ta sama lista jest w `--help`, a podkomenda
`check` pokazuje, które opcje są aktywne dla podanych argumentów).

Pełna, zawsze aktualna lista opcji: `nacelle-translator --help` (generuje ją
kod, więc nie ma jak się rozjechać). Poniżej rozszerzony opis — trafiają tu
wyłącznie opcje, które wymagają dostrojenia kluczami w pliku konfiguracyjnym:

- **`speculative-stt`** — spekulacyjne STT (LocalAgreement-2): whisper jest
  puszczany co `stt.cadence_ms` czasu audio na rosnącym, **otwartym**
  segmencie, a stabilny prefiks dwóch zgodnych przebiegów idzie do
  tłumaczenia, zanim VAD domknie segment. Daje wyraźnie niższe opóźnienie
  pierwszych słów kosztem dodatkowych przebiegów whispera (przy CPU-only
  potrafi tor przeciążyć — wtedy nie włączaj). Klucze `cadence_ms`,
  `min_open_ms` i `min_fragment_chars` w sekcji `[stt]` **stroją** tę
  funkcję, ale jej nie włączają; przy włączonej spekulacji opłaca się też
  podnieść `vad.soft_max_ms` (patrz komentarz w
  `nacelle-translator.toml.example`).

Każde uruchomienie wypisuje na starcie linię `OPCJE EKSPERYMENTALNE: …`
(z listą albo z `brak`) — po to, by log z sesji dało się jednoznacznie
przypisać do wariantu, także wtedy, gdy wklejasz sam fragment. Wariant
z opcjami leci na poziomie `WARN`, więc przeżywa `RUST_LOG=warn`; szczegółowy
opis każdej włączonej opcji idzie osobną linią `INFO`.

## Urządzenie wyjściowe: nic nie ustawiasz

**Nie wybieraj translatora jako wyjścia dźwięku.** Wybierz swoje słuchawki,
głośniki czy HDMI — tak jak zawsze, w Ustawieniach systemowych → Dźwięk albo
`wpctl set-default <id>`. Translator wepnie się w ten tor sam.

Działa to, bo węzeł zgłasza się WirePlumberowi jako **inteligentny filtr bez
zdefiniowanego celu** (`filter.smart = true`, bez `filter.smart.target`).
Taki filtr wpina się w *aktualnie domyślne* wyjście, a gdy je zmienisz,
WirePlumber przepina go razem z resztą — bez restartu programu. To ten sam
mechanizm, z którego korzysta EasyEffects.

Konsekwencje, o których warto wiedzieć:

- **Zmiana urządzenia w trakcie pracy jest ścieżką normalną.** Przełączanie
  słuchawek/głośników w aplecie nie kończy translatora i nie wymaga
  restartu; w logu zobaczysz `INFO domyślne wyjście dźwięku zmieniono na …
  — przelotka podąża za tym wyborem sama`.
- **Aplet głośności KDE ukrywa węzeł translatora** (`node.virtual = true`
  + `filterVirtualDevices` w plasma-pa) — i to jest zamierzone. Przelotka nie
  jest urządzeniem do wybrania.
- **Jeśli mimo to ustawisz translator jako domyślne wyjście, ucichnie.**
  Jego własny strumień odtwarzania trafiłby wtedy z powrotem na niego samego,
  a WirePlumber słusznie odmawia takiego linkowania. Program tego **nie
  naprawia sam** — nie nadpisuje ustawień dźwięku użytkownika — tylko krzyczy
  w logu (`ERROR domyślnym wyjściem dźwięku jest NASZ własny węzeł …`)
  i podaje nazwę urządzenia do wybrania. Wykrywa to też `check`.
- Klucz `[audio].output_device` jest **wycofany** i nic nie robi. Zostaje
  przyjmowany, żeby stare pliki konfiguracyjne dalej się wczytywały; program
  raz ostrzega w logu i prosi o usunięcie linii.

## Tłumaczenie: włącznik `[audio].translate`

Przelotka stoi w torze **całego** dźwięku systemu, więc mielenie wszystkiego
przez AI musi być świadomym wyborem, a nie zachowaniem domyślnym:

```toml
[audio]
translate = true    # domyślnie false
```

- `false` (domyślnie) — węzeł jest czystą przelotką. Dźwięk przechodzi
  bez zmian, tor AI nie dostaje ani jednej próbki, GPU stoi.
- `true` — do VAD trafia wszystko, co przez przelotkę leci: gra,
  powiadomienie, muzyka. Whisper miele to na GPU, lektor odzywa się w środku
  rozgrywki, a ducking ścisza **cały** system o ~14 dB przy każdej jego
  wypowiedzi — także wtedy, gdy tłumaczy zupełnie inną aplikację.

Tryb pracy jest wypisywany w logu przy każdym starcie i przez `check`.

## Zabezpieczenie przed pętlą

Gdy strumień wyjściowy translatora miałby trafić z powrotem do jego własnego
sinka, chronią przed tym:

- **wspólna `node.link-group` na obu węzłach** — jedyny zamek niezależny od
  jakiejkolwiek naszej decyzji o celu: WirePlumber odmawia linkowania węzłów
  o tej samej wartości i rekurencyjnie (do 8 hopów) wykrywa pętle pośrednie.
  Sprawdzane na każdej ścieżce wyboru celu, także przy celu domyślnym;
- **pomijanie inteligentnych filtrów jako celów** — WirePlumber nie wskaże
  jednego smart-filtra jako celu drugiego, więc nie wpiszemy się w siebie ani
  w EasyEffects „na krzyż";
- **wykrycie i głośny komunikat** w jedynym przypadku, którego nie da się
  zablokować od strony programu: gdy użytkownik ustawi translator jako
  domyślne wyjście (opis wyżej).

Czego tu **celowo nie ma**, a bywało wcześniej: `target.object`
i `node.dont-fallback` (przypinały do jednego sprzętu i kazały WirePlumberowi
niszczyć nasz węzeł) oraz `DONT_RECONNECT` (po pierwszym zlinkowaniu filtr
nigdy nie przeniósłby się na nowe wyjście — wprost sprzecznie z modelem
przelotki).

## Diagnostyka w logu

Domyślny poziom to `info`; `-v` podnosi do `debug`. Co warto rozpoznać:

- `WARN przepełnienie ringów RT ...` — tor AI albo odtwarzanie nie nadąża
  i próbki przepadają. Przy zdrowej pracy ta linia nie pada w ogóle.
- `INFO domyślne wyjście dźwięku zmieniono na ... — przelotka podąża za tym
  wyborem sama` — przełączyłeś urządzenie w KDE i wszystko jest w porządku;
  demon żyje dalej, restart nie jest potrzebny.
- `ERROR domyślnym wyjściem dźwięku jest NASZ własny węzeł ...` — ustawiłeś
  translator jako wyjście dźwięku. Wybierz swój prawdziwy sprzęt (komunikat
  podaje jego nazwę); do tego czasu translator jest niemy.
- `WARN <strumień>: błąd sesyjny od serwera ...` — rutynowy komunikat
  WirePlumbera (zwykle „no target node available"), typowy w trakcie
  przepinania urządzenia; program pracuje dalej, komunikat jest dławiony
  do jednego na 10 s.
- `ERROR ... węzeł wypadł z grafu — serwer PipeWire zniszczył nasz węzeł` —
  koniec pracy; padło połączenie z demonem. To NIE jest reakcja na zniknięcie
  urządzenia: przejściowy brak celu (przełączanie profilu karty, uśpione
  słuchawki) jest ścieżką normalną i kończy się `WARN`-em wyżej.
- `DEBUG passthrough: zaległość min/max/ost. ... (kwant ...)` — przyrząd
  pomiarowy toru RT, widoczny tylko z `-v`. Służy do dobrania rozmiaru
  bufora wstępnego; nie wpływa na ani jedną próbkę.

## Ograniczenia (v0.1)

- STT idzie na **CUDA** (feature `cuda` jest domyślnie włączona — bez CUDA
  Toolkit w środowisku budowania build w ogóle nie przejdzie, patrz
  „Budowanie z CUDA"). Ograniczeniem jest więc przepustowość GPU dzielona
  z tym, co równolegle na nim liczysz (gra, `llama-server`), a nie CPU:
  whisper `small` przy ciągłej gęstej mowie może nie nadążać — zaległe
  segmenty są wtedy sklejane w jedno wywołanie, a w ostateczności odrzucane
  (widać to w logach).
- Ducking reaguje na obecność głosu lektora, nie na to, *co* w oryginale
  jest mową — cała ścieżka (muzyka też) jest przyciszana, gdy lektor mówi.
- Napisy/OSD nie są generowane — tylko dźwięk.

## Licencja

MIT (patrz [LICENSE](LICENSE)). Zależności: pipewire-rs (MIT), whisper-rs /
whisper.cpp (MIT), rubato (MIT/Apache-2.0), ringbuf (MIT/Apache-2.0),
voice_activity_detector (MIT, model Silero — MIT), hound (Apache-2.0).
Piper (MIT, wydanie 2023.11.14) jest wywoływany jako osobny proces — nie
aktualizować do serii 1.6.x z OHF-Voice/piper1-gpl bez świadomej decyzji
(nowe wydania są GPL-3.0).

[whisper.cpp]: https://github.com/ggml-org/whisper.cpp
[piper]: https://github.com/rhasspy/piper
[rhasspy/piper — Releases]: https://github.com/rhasspy/piper/releases
[rhasspy/piper-voices]: https://huggingface.co/rhasspy/piper-voices
