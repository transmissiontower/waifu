# assets/voices

One folder per voice: `assets/voices/<name>/`. Select it in `config/settings.yaml` (`tts.voice: <name>`)
or per character with `voice: <name>` in the persona file.

| engine (`tts.engine`) | what the folder must contain |
|---|---|
| `chatterbox` (default) / `f5` | `ref.wav` - clean speech of ONE speaker, 8-12 s (must be longer than 5 s). Cloned on the fly. No folder / no `ref.wav` -> the engine's built-in voice. |
| `piper` | a Piper or Kokoro model folder packaged for sherpa-onnx (`*.onnx`, `tokens.txt`, ...) plus an optional `voice.yaml` (`speaker: 0`, `speed: 1.0`). `scripts/download-models.*` fetches `amy`. |

With no usable voice at all she falls back to a babble sound so lip-sync still works.
