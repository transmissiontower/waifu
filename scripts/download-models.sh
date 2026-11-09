#!/usr/bin/env bash
# Downloads free models/assets. Idempotent: skips what is already present. No accounts, no keys.
# Usage: scripts/download-models.sh [--small]   (--small: smaller Whisper model, for CPU-only machines)
set -u
cd "$(dirname "$0")/.."
mkdir -p assets/models assets/voices assets/avatars data

# what the loading screen shows (only if the launcher created data/boot)
detail() { [ -d data/boot ] && { printf 'phase=models\nlabel=Downloading models\ndetail=%s\n' "$1" > data/boot/phase.tmp && mv data/boot/phase.tmp data/boot/phase.txt; } || true; }

HAS_GPU=0
if command -v nvidia-smi >/dev/null 2>&1 && nvidia-smi -L >/dev/null 2>&1; then HAS_GPU=1; fi
WHISPER="ggml-base.en.bin"
if [ "$HAS_GPU" = 0 ] || [ "${1:-}" = "--small" ]; then WHISPER="ggml-tiny.en.bin"; fi
echo "[models] GPU detected: $HAS_GPU -> whisper model: $WHISPER"

fetch() { # url dest
  [ -s "$2" ] && { echo "[models] have $2"; return 0; }
  echo "[models] downloading $2"; detail "downloading $(basename "$2")"
  curl -fL --retry 3 --connect-timeout 20 -o "$2.part" "$1" && mv "$2.part" "$2" || { rm -f "$2.part"; echo "[models] WARNING: could not download $2 (continuing)"; return 1; }
}

# 1. Speech-to-text: whisper.cpp ggml model (MIT). Mirrors tried in order.
if [ ! -s "assets/models/$WHISPER" ]; then
  fetch "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/$WHISPER" "assets/models/$WHISPER" \
   || fetch "https://hf-mirror.com/ggerganov/whisper.cpp/resolve/main/$WHISPER" "assets/models/$WHISPER" \
   || echo "[models] STT model missing -> voice input disabled until you put $WHISPER into ./assets/models (text chat works)"
fi
# tell stt-service which one we picked (env override read at start)
echo "$WHISPER" > assets/models/.stt-model

# 2. Voice: Piper 'amy' (US English, public domain-ish dataset licence; see MODEL_CARD) packaged for sherpa-onnx.
if [ ! -d assets/voices/amy ]; then
  T=$(mktemp -d)
  if fetch "https://github.com/k2-fsa/sherpa-onnx/releases/download/tts-models/vits-piper-en_US-amy-low.tar.bz2" "$T/amy.tar.bz2"; then
    tar xjf "$T/amy.tar.bz2" -C "$T" && mv "$T/vits-piper-en_US-amy-low" assets/voices/amy
    printf 'speaker: 0\nspeed: 1.0\n' > assets/voices/amy/voice.yaml
  fi
  rm -rf "$T"
fi

# 3. Avatar: Seed-san (VRM sample, CC0, by pixiv/VRM Consortium)
fetch "https://raw.githubusercontent.com/vrm-c/vrm-specification/master/samples/Seed-san/vrm/Seed-san.vrm" "assets/avatars/Seed-san.vrm"

# 4. LLM via Ollama (optional): the model comes from config/settings.yaml (llm.model); "auto"/unset -> gemma3:4b.
llm_setting() { # llm_setting <key>: value of a key inside the top-level `llm:` block of settings.yaml
  awk -v key="$1" '/^llm:/{b=1;next} /^[^[:space:]#]/{b=0} b && $1==key":"{sub(/^[[:space:]]*[A-Za-z_]+:[[:space:]]*/,""); sub(/[[:space:]]*#.*/,""); gsub(/["\047]/,""); print; exit}' config/settings.yaml 2>/dev/null
}
LLM_PROVIDER="$(llm_setting provider)"
LLM="$(llm_setting model)"; { [ -z "$LLM" ] || [ "$LLM" = "auto" ]; } && LLM="gemma3:4b"
if [ "$LLM_PROVIDER" = "openai" ]; then
  echo "[models] llm.provider is openai - not pulling an Ollama model"
elif command -v ollama >/dev/null 2>&1; then
  echo "[models] ollama model: $LLM"
  echo "$LLM" > assets/models/.llm-model
  if ! curl -s -m 2 http://127.0.0.1:11434/api/tags >/dev/null 2>&1; then
    detail "starting Ollama"
    (OLLAMA_NUM_PARALLEL=2 OLLAMA_KEEP_ALIVE=-1 OLLAMA_FLASH_ATTENTION=1 ollama serve >/dev/null 2>&1 &)
    for _ in $(seq 1 75); do curl -s -m 1 http://127.0.0.1:11434/api/tags >/dev/null 2>&1 && break; sleep 0.2; done  # ready as soon as it answers (was: sleep 3)
  fi
  # pull only when the model is missing: no registry round-trip on every launch, and it works offline
  if ollama show "$LLM" >/dev/null 2>&1; then
    echo "[models] have ollama model $LLM"
  else
    detail "downloading language model $LLM"
    ollama pull "$LLM" || echo "[models] could not pull $LLM - she will use the built-in fallback brain until Ollama has a model"
  fi
else
  echo "[models] Ollama not found. Install it from https://ollama.com for real conversations (the app still runs with a tiny built-in fallback brain)."
fi
echo "[models] done"
