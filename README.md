# nacelle-translator

Węzeł PipeWire, który **tłumaczy w locie dźwięk w drodze do urządzenia
wyjściowego**. W grafie pojawia się urządzenie „Nacelle Translator (PL)" — wszystko,
co aplikacje w nie grają, jest segmentowane, transkrybowane, tłumaczone na
polski i czytane głosem lektora, a oryginał przechodzi dalej przyciszany pod
lektorem (ducking, jak w telewizyjnym szeptance).

> **Uwaga o autorstwie:** ten projekt został w całości zaprojektowany
> i napisany przez **Claude (Anthropic)** — model Claude Fable 5 działający
> w Claude Code — na zlecenie właściciela repozytorium.

## Architektura

```
aplikacje ──▶ [Nacelle Translator (PL)]  (wirtualny sink, media.class=Audio/Sink)
                    │ RT: ringbuffery SPSC (zero blokad w wątku danych)
                    ├─ passthrough stereo ────────────────────┐
                    └─ downmix mono 48 kHz                    │
                         └─▶ rubato 48k→16k ─▶ Silero VAD     │ ducking
                              └─▶ segmenter (histereza,       │ (-14 dB pod
                                   hangover 800 ms,           │  lektorem)
                                   cięcie w dołku p.)         ▼
                                   └─▶ whisper.cpp ─▶ llama-server ─▶ piper
                                     (STT, CUDA)    (tłumaczenie)   (głos pl)
                                                                    │
         głośniki/słuchawki ◀── [nacelle-translator-out] ◀── mikser ┘
                                 (strumień Playback → sprzęt)
```

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
  tmpfs) z głosem `pl_PL-gosia-medium`, który masz już w
  `~/.local/share/piper`.
- **VAD:** Silero V5 (`voice_activity_detector`) — VAD energetyczny odpada,
  bo muzyka pod mową trzyma bramkę otwartą.
- Wypowiedzi już po polsku są rozpoznawane i **pomijane** (przechodzi sam
  oryginał, bez duckingu).

Opóźnienie z natury rzeczy: lektor odzywa się kilka sekund po oryginale
(segmentacja per wypowiedź + STT na CPU + tłumaczenie + synteza; przy
lokalnym modelu Ollamy pierwsza wypowiedź po dłuższej ciszy płaci dodatkowo
czas ładowania modelu do pamięci — `nacelle-translator` rozgrzewa go na starcie,
żeby zminimalizować ten koszt w trakcie sesji). Oryginał gra na bieżąco,
więc obraz się nie rozjeżdża — spóźnia się tylko głos lektora.

## Wymagania

Wszystko poza modelem whispera już jest na tym systemie:

- PipeWire (daemon) + nagłówki `libpipewire-0.3` — **z Homebrew**
  (`brew install pipewire`; wersja zgodna z systemową 1.6.8),
- Rust, cmake, clang/libclang — z Homebrew,
- piper + polski głos w `~/.local/share/piper` (binarka ma RUNPATH=$ORIGIN,
  żadnych zmiennych środowiskowych nie trzeba),
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

`ggml-small` to rozsądny kompromis na CPU. Szybciej (kosztem jakości):
`ggml-base.bin`. **Nie używaj modeli `*.en`** — są angielsko-tylko i nie
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
sudo make install     # kopiuje dist/ + models/ do /opt/nacelle-translator (bin/, lib/, models/);
                      # usuwa ./target przed i po instalacji
sudo make uninstall
```

`make install` tylko kopiuje pliki — nie kompiluje niczego jako root. Zbuduj
i spakuj (`make build && make dist`, albo bezpośrednio `./build.sh` +
`./package.sh`) jako zwykły użytkownik wewnątrz distroboxa, dopiero potem
`sudo make install`.

### Silnik `llamacpp` — llama-server + TranslateGemma-4B-IT

Domyślny silnik tłumaczenia (`engine = "llamacpp"`) potrzebuje osobno
uruchomionego `llama-server` (część projektu [llama.cpp], budowanego też z
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
./target/release/nacelle-translator devices   # lista węzłów Audio/Sink (node.name do configu)
./target/release/nacelle-translator           # start; Ctrl+C kończy
```

Potem ustaw „Nacelle Translator (PL)" jako wyjście dźwięku — w ustawieniach
dźwięku KDE albo:

```sh
wpctl status                 # znajdź id sinka "Nacelle Translator (PL)"
wpctl set-default <id>
```

Konfiguracja: skopiuj `nacelle-translator.toml.example` do `nacelle-translator.toml` (albo
uruchamiaj bez pliku — obowiązują te same wartości domyślne). Tryb testowy
toru audio bez API: `engine = "off"` w sekcji `[translate]` (lektor czyta
oryginalny, nieprzetłumaczony tekst).

## Wybór urządzenia wyjściowego

Bez `output_device` w konfiguracji program **odczytuje** (nigdy nie zapisuje)
z metadanych PipeWire, jakie urządzenie masz **aktualnie ustawione jako
domyślne wyjście** — to samo, co `wpctl status` pokazuje pod „Default
Configured Devices" — i tam kieruje swój strumień odtwarzania. Program
niczego w ustawieniach systemowych nie zmienia; jedyna zmiana, jaką trzeba
wykonać ręcznie, to (jednorazowo) wybranie „Nacelle Translator (PL)" jako
urządzenia wyjściowego w KDE albo przez `wpctl set-default` — PipeWire
zapamiętuje ten wybór trwale, więc przy kolejnych uruchomieniach programu
nie trzeba tego powtarzać.

## Zabezpieczenie przed pętlą

Gdy „Nacelle Translator (PL)" jest domyślnym wyjściem, strumień wyjściowy
translatora **nie może** trafić z powrotem do niego. Chronią przed tym:
`target.object` wskazujący konkretny sprzętowy `node.name`, flaga
`DONT_RECONNECT` (po zniknięciu celu strumień pada zamiast wracać do
domyślnego sinka) i wspólna `node.link-group` na obu węzłach — wzorzec
z `module-loopback`.

## Ograniczenia (v0.1)

- Tor na CPU; whisper `small` przy ciągłej gęstej mowie może nie nadążać —
  zaległe segmenty są wtedy sklejane w jedno wywołanie, a w ostateczności
  odrzucane (widać to w logach). GPU (Vulkan/CUDA) wymagałoby doinstalowania
  toolchainu — featury `whisper-rs` są na to gotowe.
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
