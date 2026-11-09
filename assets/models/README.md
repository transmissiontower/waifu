# assets/models

Speech-recognition models (whisper.cpp `ggml-*.bin`). Downloaded automatically by `scripts/download-models.*`
(tiny on CPU, base on a GPU) - or drop your own file here.

Pick one in `config/settings.yaml`:

```yaml
stt:
  model: ggml-small.en.bin   # or `auto` = what the download script picked
  language: en
```

Also here (written by the download script, safe to delete): `.stt-model`, `.llm-model` - the hardware-based picks
used when a model is set to `auto`.
