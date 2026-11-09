# config/personas

One YAML file per character. Pick the active one in `config/settings.yaml`:

```yaml
persona: mio          # -> config/personas/mio.yaml
```

Files are **hot-reloaded**: edit the active persona while the app runs and the next reply uses it.
Each file defines `name`, `age`, `language`, `voice`, `avatar`, `greeting`, `speech_style`, `backstory`,
`behavior_rules`, `emotions`. `voice` / `avatar` here are the character's own defaults;
`tts.voice` / `avatar.file` in `settings.yaml` override them when set.

New character: copy `mio.yaml` to `<name>.yaml`, edit, set `persona: <name>`.
