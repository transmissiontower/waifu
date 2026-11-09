# Configuration

Three layers, from "what I change daily" to "how it is wired":

| file | purpose | edit it when |
|---|---|---|
| `config/settings.yaml` | **your choices**: persona, LLM model/provider, STT model/language, TTS engine/voice, avatar, memory | you want a different model, voice, avatar... |
| `config/personas/<name>.yaml` | one character: personality, speech style, rules, default voice/avatar | you write or tweak a character (hot-reloaded) |
| `config/services.yaml` | wiring: ports, which binary/command runs each service, TTS engine catalogue | you move a port or add a service/engine |

**Precedence:** environment variables (`WAIFU_*`) > `settings.yaml` > persona file / `services.yaml`.
Every key in `settings.yaml` is optional; blank = "use the default below it". Unknown keys are rejected at start
so a typo can't silently do nothing. Changes apply on restart, except persona files (hot-reloaded).

## Recipes

**Another Ollama model** - `ollama pull gemma3:12b`, then `llm.model: gemma3:12b`.

**LM Studio / llama.cpp / vLLM / OpenAI**
```yaml
llm:
  provider: openai
  model: my-model
  openai_base_url: http://127.0.0.1:1234/v1
  openai_api_key_env: OPENAI_API_KEY   # only if the server wants a key (the NAME of the env var)
```

**Another voice** - make `assets/voices/<name>/ref.wav` (8-12 s of one clean speaker), then `tts.voice: <name>`.
**Another TTS engine** - `tts.engine: chatterbox | f5 | piper`; `tts.device: mps | cuda | cpu | auto`.
**Another avatar** - drop a `.vrm` into `assets/avatars/`, then `avatar.file: name.vrm`.
**Another character** - add `config/personas/<name>.yaml`, then `persona: <name>`.
**Speech recognition** - drop `ggml-*.bin` into `assets/models/`, set `stt.model` (+ `stt.language` for multilingual models).
**Turn a part off** - `stt.enabled: false`, `tts.enabled: false`, `memory.enabled: false` (text-only chat keeps working).

## Web search (internet access)

She can look things up (news, weather, scores, "google X"). Gemma has no tool calling, so the orchestrator
does the search itself and attaches a few result snippets to *that one* LLM request; history and memory only
ever store what was actually said. Chat that does not look like it needs fresh info (`mode: auto`) never
triggers a search, so normal reply latency is unchanged. On any error or timeout she answers without it.

```yaml
web_search:
  enabled: true
  provider: duckduckgo      # or: searxng (self-hosted, set searxng_url)
  mode: auto                # auto | always
  max_results: 4
  timeout_ms: 2500
```

Privacy: with `duckduckgo` the search words leave your machine. Use `provider: searxng` with your own
instance (`json` must be listed in its `search.formats`) to keep it all on your network.

## Environment overrides

| variable | effect |
|---|---|
| `WAIFU_LLM_MODEL` | Ollama model, beats everything |
| `WAIFU_STT_MODEL` | Whisper file name, beats everything |
| `WAIFU_OLLAMA_URL` | Ollama address (Docker) |
| `WAIFU_SETTINGS` / `WAIFU_CONFIG` | path to `settings.yaml` / `services.yaml` |
| `WAIFU_ADDR_<ROLE>`, `WAIFU_HTTP_<ROLE>` | service addresses |
| `RUST_LOG`, `LOG_FORMAT=json` | logging |

## Adding a TTS engine
Add an entry to `tts_engines:` in `services.yaml` (`impl`, `name`, `command` with `{addr}` for the listen address),
then select it with `tts.engine: <key>`. See `docs/CUSTOM_SERVICE.md`.
