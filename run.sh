#!/usr/bin/env bash
# One-command launch (Linux/macOS): installs missing toolchain pieces, builds in release mode,
# downloads models, starts every service under the supervisor and opens the avatar window.
# No Docker, no API keys, no accounts.      Usage: ./run.sh [--no-open] [--small] [extra supervisor args]
set -euo pipefail
cd "$(dirname "$0")"
OPEN="--open"; SMALL=""; ARGS=()
for a in "$@"; do case "$a" in --no-open) OPEN="";; --small) SMALL="--small";; *) ARGS+=("$a");; esac; done

say() { printf '\033[1;35m[run]\033[0m %s\n' "$*"; }
SUDO=""; [ "$(id -u)" != 0 ] && command -v sudo >/dev/null 2>&1 && SUDO="sudo"

# 1. system build tools (whisper.cpp needs cmake + a C++ compiler + libclang for bindings)
need=()
command -v cmake >/dev/null 2>&1 || need+=(cmake)
command -v c++ >/dev/null 2>&1 || command -v g++ >/dev/null 2>&1 || need+=(gxx)
command -v clang >/dev/null 2>&1 || need+=(clang)
command -v pkg-config >/dev/null 2>&1 || need+=(pkgconf)
command -v curl >/dev/null 2>&1 || need+=(curl)
command -v bzip2 >/dev/null 2>&1 || need+=(bzip2)
if [ ${#need[@]} -gt 0 ]; then
  say "installing system packages: ${need[*]}"
  if command -v apt-get >/dev/null 2>&1; then
    m=(); for n in "${need[@]}"; do case $n in gxx) m+=(g++);; pkgconf) m+=(pkg-config);; *) m+=($n);; esac; done
    $SUDO apt-get update -qq && $SUDO apt-get install -y -qq "${m[@]}" libclang-dev libasound2-dev libssl-dev
  elif command -v dnf >/dev/null 2>&1; then
    $SUDO dnf install -y cmake gcc-c++ clang clang-devel pkgconf-pkg-config curl bzip2 openssl-devel alsa-lib-devel
  elif command -v brew >/dev/null 2>&1; then
    brew install cmake llvm pkg-config curl
  else
    say "please install: ${need[*]} (cmake, a C++ compiler, clang/libclang) and re-run"; exit 1
  fi
fi

# 2. Rust toolchain via rustup
if ! command -v cargo >/dev/null 2>&1; then
  [ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
fi
if ! command -v cargo >/dev/null 2>&1; then
  say "installing Rust toolchain with rustup"
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
  . "$HOME/.cargo/env"
fi
say "using $(cargo --version)"

# 3. loading screen: a tiny dependency-free server that is alive while everything below compiles / downloads / starts.
#    Strictly optional - if it cannot be built, everything works as before (without the loading screen).
BOOT="data/boot"; rm -rf "$BOOT"; mkdir -p "$BOOT"
phase() { printf 'phase=%s\nlabel=%s\ndetail=%s\n' "$1" "$2" "${3:-}" > "$BOOT/phase.tmp" && mv "$BOOT/phase.tmp" "$BOOT/phase.txt"; }
fail() { # fail <phase> <label> <message>: show the error on the loading screen, keep it up for a while
  printf 'phase=%s\nlabel=%s\nerror=%s\n' "$1" "$2" "$3" > "$BOOT/phase.tmp" && mv "$BOOT/phase.tmp" "$BOOT/phase.txt"
  say "$3"; sleep 30; exit 1
}
LAUNCHER_PID=""
trap '[ -n "$LAUNCHER_PID" ] && kill "$LAUNCHER_PID" 2>/dev/null || true' EXIT
if cargo build --release -p launcher >/dev/null 2>"$BOOT/launcher-build.log"; then
  target/release/launcher --root . --dir "$BOOT" $OPEN &
  LAUNCHER_PID=$!
else
  say "loading screen unavailable (see $BOOT/launcher-build.log) - continuing without it"
fi

# 4. build (release)
phase build "Compiling the engine"
say "building (first build takes a while: whisper.cpp + sherpa-onnx)"
cargo build --release --workspace --exclude e2e --exclude launcher 2>&1 | tee "$BOOT/build.log" || fail build "Compiling the engine" "The build failed - see the log above"

# shared libraries sherpa-onnx needs at runtime -> next to the binaries
for lib in $(find target/release/build ~/.cache/sherpa-rs -name '*.so*' -o -name '*.dylib' 2>/dev/null | grep -E 'onnxruntime|sherpa-onnx' || true); do
  cp -n "$lib" target/release/ 2>/dev/null || true
done

# 5. models & assets
phase models "Downloading models"
./scripts/download-models.sh $SMALL || say "model download had problems - continuing in degraded mode"

# 6. frontend (rebuild when the bundle is missing or the sources are newer)
if command -v npm >/dev/null 2>&1 && { [ ! -f frontend/dist/app.js ] || [ -n "$(find frontend/src -newer frontend/dist/app.js 2>/dev/null | head -1)" ]; }; then
  phase frontend "Building the interface"
  say "rebuilding frontend"; (cd frontend && npm install --no-audit --no-fund --loglevel=error && npm run build) || say "frontend build failed - using the existing bundle if any"
fi

# 7. go. (No `exec`: the shell stays around to stop the loading screen when the app exits.)
#    With a loading screen it opens the window itself and hands over to the app when everything is ready.
phase services "Starting services"
say "starting - Ctrl+C stops everything"
SUP_OPEN="$OPEN"; [ -n "$LAUNCHER_PID" ] && SUP_OPEN=""
target/release/supervisor $SUP_OPEN ${ARGS[@]+"${ARGS[@]}"}
