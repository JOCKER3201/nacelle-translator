#!/usr/bin/env bash
# Budowanie nacelle-translator na Bazzite (system niemutowalny):
# nagłówki PipeWire i libclang pochodzą z Homebrew — zero zmian w systemie.
#
#   ./build.sh          czysty build (cargo clean + build --release)
#   ./build.sh --fast   bez czyszczenia (szybsza iteracja; uwaga: crate `ort`
#                       pobiera przy pełnym buildzie ~50 MB onnxruntime z sieci)
set -euo pipefail
cd "$(dirname "$0")"

case "${1:-}" in
  "") CLEAN=1 ;;
  --fast) CLEAN=0 ;;
  *)
    echo "nieznany argument: $1 (dozwolone: --fast)" >&2
    exit 2
    ;;
esac
if (( $# > 1 )); then
  echo "nadmiarowe argumenty: ${*:2}" >&2
  exit 2
fi

# Nagłówki PipeWire i libclang: na hoście (Bazzite) z Homebrew; w kontenerze
# budującym (distrobox "Fedora", build CUDA) z pakietów systemowych
# (pipewire-devel, clang-devel) — wtedy Homebrew nie jest ani dostępny,
# ani potrzebny.
if pkg-config --exists 'libpipewire-0.3 >= 0.3' 2>/dev/null; then
  : # systemowe nagłówki już widoczne — nic nie trzeba ustawiać
else
  BREW="${HOMEBREW_PREFIX:-$(brew --prefix 2>/dev/null || echo /home/linuxbrew/.linuxbrew)}"
  [[ -d "$BREW/lib/pkgconfig" ]] || {
    echo "błąd: pkg-config nie zna libpipewire-0.3, a nie ma też $BREW/lib/pkgconfig —" >&2
    echo "  na hoście: brew install pipewire (albo ustaw HOMEBREW_PREFIX)" >&2
    echo "  w kontenerze: dnf install pipewire-devel" >&2
    exit 1
  }
  export PATH="$BREW/bin:$PATH"
  export PKG_CONFIG_PATH="$BREW/lib/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
  export LIBCLANG_PATH="${LIBCLANG_PATH:-$BREW/opt/llvm/lib}"
fi
if [[ -z "${LIBCLANG_PATH:-}" ]]; then
  for d in /usr/lib64 /usr/lib; do
    [[ -e "$d"/libclang.so || -n "$(compgen -G "$d/libclang.so.*")" ]] && { export LIBCLANG_PATH="$d"; break; }
  done
fi
[[ -n "${LIBCLANG_PATH:-}" ]] || {
  echo "błąd: nie znalazłem libclang — zainstaluj 'brew install llvm' / 'dnf install clang-devel' albo ustaw LIBCLANG_PATH" >&2
  exit 1
}
export CC="${CC:-clang}" CXX="${CXX:-clang++}"                # whisper.cpp (cmake)

# Backend CUDA (whisper-rs feature "cuda", zawsze włączona w Cargo.toml) —
# wymaga CUDA Toolkit (nvcc), dostępnego tylko w kontenerze budującym
# (distrobox "Fedora"), nie na hoście Bazzite. Trzy rzeczy, które inaczej
# trzeba by ustawiać ręcznie przed KAŻDYM buildem:
CUDA_BIN="${CUDA_BIN:-/usr/local/cuda/bin}"
if [[ -x "$CUDA_BIN/nvcc" ]]; then
  case ":$PATH:" in *":$CUDA_BIN:"*) ;; *) export PATH="$CUDA_BIN:$PATH" ;; esac
else
  echo "błąd: brak nvcc w $CUDA_BIN — feature \"cuda\" (Cargo.toml) wymaga CUDA Toolkit." >&2
  echo "  Uruchom w kontenerze z Toolkitem, np.: distrobox enter Fedora -- $0 ${*:-}" >&2
  echo "  (albo ustaw CUDA_BIN na katalog z nvcc)" >&2
  exit 1
fi
# nvcc bywa niekompatybilny z najnowszym systemowym gcc/g++ (np. Fedora
# Rawhide w distroboxie "Fedora") — wymaga jawnie wskazanego, starszego
# kompilatora hosta; g++-15 zweryfikowany jako działający z CUDA 13.3.
if [[ -z "${CMAKE_CUDA_HOST_COMPILER:-}" ]]; then
  for cxx in /usr/bin/g++-15 /usr/bin/g++-14 /usr/bin/g++-13; do
    [[ -x "$cxx" ]] && { export CMAKE_CUDA_HOST_COMPILER="$cxx"; break; }
  done
fi
export CMAKE_CUDA_ARCHITECTURES="${CMAKE_CUDA_ARCHITECTURES:-120}"  # RTX 5090 (Blackwell)
echo "CUDA: nvcc=$CUDA_BIN/nvcc host-cc=${CMAKE_CUDA_HOST_COMPILER:-<auto>} arch=${CMAKE_CUDA_ARCHITECTURES}"

if [[ "$CLEAN" == "1" ]]; then
  cargo clean
fi
cargo build --release

echo
echo "gotowe: target/release/nacelle-translator"
echo "następny krok: ./target/release/nacelle-translator check"
