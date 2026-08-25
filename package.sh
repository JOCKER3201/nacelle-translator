#!/usr/bin/env bash
# Pakuje zbudowaną binarkę CUDA razem z wymaganymi bibliotekami runtime
# (libcudart/libcublas/libcublasLt) do katalogu dist/ — whisper-rs i
# llama.cpp domyślnie linkują się z nimi DYNAMICZNIE, a to biblioteki z
# CUDA Toolkit, nie ze sterownika (bazzite-nvidia-open daje tylko
# libcuda.so). Bez tego kroku gotowa binarka nie wystartuje na maszynie,
# która ma sam sterownik NVIDIA (patrz RPATH $ORIGIN/../lib w
# .cargo/config.toml — stąd binarka szuka lib/ obok siebie po instalacji).
#
# Uruchamiaj PO ./build.sh, wewnątrz distroboxa "Fedora" (tam jest CUDA
# Toolkit, więc `ldd` znajdzie prawdziwe ścieżki bibliotek).
set -euo pipefail
cd "$(dirname "$0")"

BIN="target/release/nacelle-translator"
CUDA_LIB_NAMES=(libcudart.so libcublas.so libcublasLt.so)

[[ -f "$BIN" ]] || {
  echo "błąd: brak $BIN — najpierw ./build.sh" >&2
  exit 1
}

# bin/ + lib/ obok siebie — ten sam układ co po instalacji do /opt, żeby
# RUNPATH=$ORIGIN/../lib działał także przy uruchamianiu prosto z dist/
rm -rf dist
mkdir -p dist/bin dist/lib

for name in "${CUDA_LIB_NAMES[@]}"; do
  real="$(ldd "$BIN" | awk -v n="$name" '$1 ~ n {print $3; exit}')"
  [[ -n "$real" && -f "$real" ]] || {
    echo "błąd: nie znaleziono $name w zależnościach $BIN (ldd) — build z --features cuda w środowisku z CUDA Toolkit?" >&2
    exit 1
  }
  cp -L "$real" dist/lib/
  echo "skopiowano: $real -> dist/lib/$(basename "$real")"
done

cp "$BIN" dist/bin/nacelle-translator
# llama-server (silnik "llamacpp"): budowany statycznie (BUILD_SHARED_LIBS=OFF
# + GGML_STATIC=ON + LLAMA_OPENSSL=OFF), więc żadnych bibliotek nie dokłada —
# w runtime potrzebuje tylko libcuda.so ze sterownika
LLAMA_SERVER="llama.cpp/build/bin/llama-server"
if [[ -f "$LLAMA_SERVER" ]]; then
  cp "$LLAMA_SERVER" dist/bin/llama-server
  echo "skopiowano: $LLAMA_SERVER -> dist/bin/llama-server"
else
  echo "uwaga: brak $LLAMA_SERVER — dist/ bez silnika llamacpp (patrz README)" >&2
fi
# launcher całego stosu (llama-server w tle + nacelle-translator) — trafia
# do bin/ obok binarek, bez rozszerzenia
cp translator dist/bin/translator
chmod 755 dist/bin/translator
cp nacelle-translator.toml.example README.md dist/
# rzeczywisty config (gitignored — engine=llamacpp, tuning VAD/TTS z tej
# sesji) też do dist/, żeby instalacja miała działającą konfigurację, nie
# tylko przykład wymagający ręcznego skopiowania
if [[ -f nacelle-translator.toml ]]; then
  cp nacelle-translator.toml dist/
  echo "skopiowano: nacelle-translator.toml (Twoja konfiguracja) -> dist/"
fi

echo
echo "gotowe: dist/ (binarka + ${#CUDA_LIB_NAMES[@]} biblioteki CUDA runtime)"
echo "następny krok: sudo make install"
