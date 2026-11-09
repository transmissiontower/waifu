# assets/avatars

VRM avatars (`.vrm`, VRM 0.x or 1.0). `Seed-san.vrm` (CC0) is downloaded by `scripts/download-models.*`.

Use one:

* `config/settings.yaml` -> `avatar: { file: my-model.vrm }` (wins over everything), or
* `avatar: my-model.vrm` in the persona file `config/personas/<name>.yaml` (per character), or
* the drop-down in the app header (this session only).

Different model, odd arms or camera? Arm angles and framing live in `frontend/src/avatar.ts`
(`ARM_DOWN`, camera position), the emotion -> face table is `EMOTIONS` in the same file.
