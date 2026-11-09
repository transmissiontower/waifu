# Writing a custom service (any language)

Every service is just a **gRPC server implementing a contract** from `backend/crates/contracts/proto/`.
Swap one in by editing `config/services.yaml` - no Rust, no rebuild.

| role (config key) | proto | RPCs you implement |
|---|---|---|
| `llm` | `llm.proto` | `Chat` (server-stream of tokens), `Embed`, `Info` |
| `stt` | `stt.proto` | `Transcribe` (PCM16 16 kHz mono -> text) |
| `tts` | `tts.proto` | `Synthesize` (server-stream of PCM16 chunks), `ListVoices` |
| `memory` | `memory.proto` | `Recall`, `StoreTurn`, `ExtractFacts`, `ListFacts`, `Summarize` |
| `persona` | `persona.proto` | `Get`, `BuildPrompt`, `Watch` (server-stream) |
| `avatar` | `avatar.proto` | `Publish`, `Subscribe` (server-stream) |
| `orchestrator` | `orchestrator.proto` | `Converse` (bidi-stream) - the gateway talks to this |

## 5-minute walkthrough: a Python TTS

A complete, working example lives in `backend/engines/template-python/tts_service.py`
(it generates the Python stubs from the shared `.proto` files on first start).

1. `pip install grpcio grpcio-tools`
2. Copy the template, replace `synthesize_pcm()` with your engine. Return little-endian **PCM16 mono** and set
   `sample_rate` on each `AudioChunk` (any rate; the browser resamples).
3. Point the config at it:

```yaml
services:
  tts:
    impl: external                 # supervisor will NOT look for a builtin binary
    name: my-python-tts            # shown in /api/status
    addr: 127.0.0.1:60001          # gRPC address of your server
    command: ["python3", "backend/engines/template-python/tts_service.py", "--addr", "127.0.0.1:60001"]
```

`command` is optional: with it, the supervisor starts, health-probes (TCP connect to `addr`) and restarts your
process with backoff, and shows its logs prefixed `[tts]`. Without it, start the service yourself - the rest of the
system reconnects automatically (channels are lazy, calls have timeouts, circuit breakers protect callers).

Environment passed to supervised processes: `WAIFU_CONFIG`, `WAIFU_ROLE`, `WAIFU_ADDR`, plus anything under `env:`.

This exact swap is exercised by the end-to-end test: `WAIFU_E2E_EXTERNAL_TTS=1 cargo test -p e2e`.

## Other languages
Generate stubs from the same files: `protoc -I backend/crates/contracts/proto --<lang>_out=... --grpc_<lang>_out=... tts.proto common.proto`
(Node: `@grpc/proto-loader`; Go: `protoc-gen-go-grpc`; C#: `Grpc.Tools`; ...).

## Contract rules of the road
* **Streaming**: `Chat` must stream tokens as they are produced and stop quickly when the client cancels the call
  (barge-in). `Synthesize` should stream chunks of ~100-300 ms. Check `context.is_active()` / cancellation.
* **Emotion tags**: the orchestrator parses `[happy]`-style tags out of the LLM text, so your LLM only has to follow
  the system prompt it receives. TTS receives clean text.
* **Errors**: return gRPC `UNAVAILABLE` with a human-readable message when you are degraded - the orchestrator shows it
  to the user and falls back (text-only, no memory, built-in persona...).
* **Trace ids**: read the `x-trace-id` gRPC metadata header and include it in your logs to correlate a request across services.
* **Health**: builtin services expose `GET /healthz`, `/metrics` on their `http:` address. External services are probed
  with a TCP connect; add an `http:` address and `/healthz` yourself if you want richer status (`impl: external` +
  `http:` is only probed by TCP today).
* **Loading screen**: if your service needs a while to load (a model, a warm-up), print `@@loading <what you are doing>` on
  stdout *before* you open your port and `@@ready` when done (or `@@failed <why>`). The supervisor shows it on the loading
  screen and waits for `@@ready` before it says the app is up. Services that print nothing are ready as soon as they are healthy.
* **Versioning**: contracts are additive - new fields get new numbers; never reuse or renumber.

## Replacing the orchestrator / gateway
Both are replaceable too (`orchestrator.proto` is the only thing the gateway needs). The WebSocket protocol between
browser and gateway is documented at the top of `backend/services/gateway/src/ws.rs`.

## F5-TTS (shipped, `backend/engines/f5-tts/`)
Select it with `tts.engine: f5` in `config/settings.yaml`. Setup once:
```
pip install torch torchaudio --index-url https://download.pytorch.org/whl/cu124   # CUDA build
pip install -r backend/engines/f5-tts/requirements.txt
```
Models download from Hugging Face on first start (~1.5 GB). Speed knobs (env): `F5_NFE` (8-12 fastest, 16 default, 32 best),
`F5_CLAUSE_CHARS` (default 70), `F5_DEVICE`. To go back to Piper: set `tts` to `bin: tts-service` and remove `impl/addr/command`.

## Chatterbox Turbo (`backend/engines/chatterbox-tts/`)
Own venv (it pins its own torch, don't share it with F5), Python 3.11:
```
python3.11 -m venv .venv-chatterbox
.venv-chatterbox/bin/python -m pip install --upgrade pip
.venv-chatterbox/bin/python -m pip install -r backend/engines/chatterbox-tts/requirements.txt
```
The service auto-switches to `.venv-chatterbox`. Voice: `assets/voices/<name>/ref.wav` (8-12 s, must be > 5 s). Tags like `[laugh]` are NOT generated by the LLM here.
