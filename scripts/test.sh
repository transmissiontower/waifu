#!/usr/bin/env bash
# Full quality gate: fmt, clippy, unit tests, then the end-to-end pipeline test (real binaries).
set -euo pipefail
cd "$(dirname "$0")/.."
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo build --workspace
cargo test --workspace
# no-toolchain tests: Chatterbox latency logic, VAD, loading-screen page logic
python3 -m unittest discover -s backend/engines/chatterbox-tts -p 'test_*.py'
node --test frontend/test/vad.test.ts backend/tools/launcher/test/splash.test.mjs
echo "all checks passed"
