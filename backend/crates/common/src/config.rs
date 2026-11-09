use anyhow::{anyhow, Context};
use serde::{de::DeserializeOwned, Deserialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub global: Global,
    pub services: BTreeMap<String, ServiceCfg>,
    /// Selectable TTS engines (`tts.engine` in settings.yaml picks one and merges it over `services.tts`).
    #[serde(default)]
    pub tts_engines: BTreeMap<String, EngineCfg>,
}

/// What a TTS engine changes in the `tts` service entry when it is selected.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct EngineCfg {
    #[serde(rename = "impl")]
    pub implementation: Option<String>,
    pub name: Option<String>,
    pub bin: Option<String>,
    pub command: Option<Vec<String>>,
    pub startup_timeout_s: Option<u64>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Global {
    #[serde(default = "d_log")]
    pub log_level: String,
    /// "inprocess" (default) or "nats"
    #[serde(default = "d_bus")]
    pub bus: String,
    pub nats_url: Option<String>,
    #[serde(default = "d_models")]
    pub models_dir: String,
    #[serde(default = "d_avatars")]
    pub avatars_dir: String,
    #[serde(default = "d_voices")]
    pub voices_dir: String,
    #[serde(default = "d_personas")]
    pub personas_dir: String,
    #[serde(default = "d_data")]
    pub data_dir: String,
    #[serde(default = "d_user")]
    pub default_user: String,
}
fn d_log() -> String {
    "info".into()
}
fn d_bus() -> String {
    "inprocess".into()
}
fn d_models() -> String {
    "assets/models".into()
}
fn d_avatars() -> String {
    "assets/avatars".into()
}
fn d_voices() -> String {
    "assets/voices".into()
}
fn d_personas() -> String {
    "config/personas".into()
}
fn d_data() -> String {
    "data".into()
}
fn d_user() -> String {
    "default".into()
}

impl Default for Global {
    fn default() -> Self {
        serde_yaml::from_str("{}").expect("defaults")
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServiceCfg {
    /// "builtin" (supervisor spawns our binary) or "external" (anything speaking the proto contract).
    #[serde(rename = "impl", default = "d_impl")]
    pub implementation: String,
    /// Human-readable implementation name, e.g. "piper-sherpa" or "my-python-tts".
    pub name: Option<String>,
    /// gRPC address host:port (for the gateway: its public HTTP address).
    pub addr: String,
    /// Health/metrics HTTP address (builtin services).
    pub http: Option<String>,
    #[serde(default = "d_true")]
    pub enabled: bool,
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Binary name for builtin services (default: derived from the role).
    pub bin: Option<String>,
    /// Seconds the supervisor waits for the first healthy check.
    #[serde(default = "d_startup")]
    pub startup_timeout_s: u64,
    /// Optional command line to launch an external service under supervision.
    pub command: Option<Vec<String>>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Service specific settings (free-form YAML).
    #[serde(default)]
    pub settings: serde_yaml::Value,
}
fn d_impl() -> String {
    "builtin".into()
}
fn d_true() -> bool {
    true
}
fn d_startup() -> u64 {
    90
}

impl Config {
    pub fn config_path() -> PathBuf {
        std::env::var("WAIFU_CONFIG").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("config/services.yaml"))
    }

    /// `WAIFU_SETTINGS` or `settings.yaml` next to the services file. A missing file simply means "no overrides".
    pub fn settings_path() -> PathBuf {
        match std::env::var("WAIFU_SETTINGS") {
            Ok(p) if !p.is_empty() => PathBuf::from(p),
            _ => Self::config_path().with_file_name("settings.yaml"),
        }
    }

    pub fn load() -> anyhow::Result<Config> {
        let p = Self::config_path();
        let text = std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
        let mut cfg = Self::parse(&text)?;
        let sp = Self::settings_path();
        if sp.is_file() {
            let st = std::fs::read_to_string(&sp).with_context(|| format!("reading {}", sp.display()))?;
            cfg.apply_settings(&UserSettings::parse(&st).with_context(|| format!("in {}", sp.display()))?);
        }
        cfg.apply_env_overrides();
        Ok(cfg)
    }

    /// `WAIFU_ADDR_<ROLE>=host:port` and `WAIFU_HTTP_<ROLE>=host:port` override addresses without editing the
    /// file (used by Docker, e.g. `WAIFU_ADDR_GATEWAY=0.0.0.0:8080`).
    pub fn apply_env_overrides(&mut self) {
        for (role, svc) in self.services.iter_mut() {
            let r = role.to_uppercase();
            if let Ok(v) = std::env::var(format!("WAIFU_ADDR_{r}")) {
                svc.addr = v;
            }
            if let Ok(v) = std::env::var(format!("WAIFU_HTTP_{r}")) {
                svc.http = Some(v);
            }
        }
    }

    pub fn parse(text: &str) -> anyhow::Result<Config> {
        serde_yaml::from_str(text).context("parsing services.yaml")
    }

    pub fn service(&self, role: &str) -> anyhow::Result<&ServiceCfg> {
        self.services.get(role).ok_or_else(|| anyhow!("service '{role}' missing from config/services.yaml"))
    }

    /// gRPC address of a service role; every cross-service call goes through this, which is what
    /// makes any service swappable by editing config/services.yaml only.
    pub fn addr(&self, role: &str) -> anyhow::Result<String> {
        Ok(self.service(role)?.addr.clone())
    }

    pub fn is_enabled(&self, role: &str) -> bool {
        self.services.get(role).map(|s| s.enabled).unwrap_or(false)
    }

    /// Deserialize the `settings:` block of a service, using defaults for anything absent.
    pub fn settings<T: DeserializeOwned + Default>(&self, role: &str) -> T {
        match self.services.get(role) {
            Some(s) if !s.settings.is_null() => serde_yaml::from_value(s.settings.clone()).unwrap_or_else(|e| {
                tracing::warn!(role, error = %e, "invalid settings, using defaults");
                T::default()
            }),
            _ => T::default(),
        }
    }
}

// ───────────────────────────── user settings (config/settings.yaml) ─────────────────────────────
//
// `services.yaml` is the wiring (ports, binaries); `settings.yaml` is what a person edits day to day:
// model, voice, avatar, persona... It is laid over the parsed services file at load time, in every
// process, so no service needs to know that it exists. Every field is optional: absent = keep the
// value from services.yaml / the persona file. Unknown keys are rejected so a typo is not silent.

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UserSettings {
    /// Persona to load: a name (`mio` -> `<personas_dir>/mio.yaml`) or a path to a .yaml file.
    pub persona: Option<String>,
    /// How she addresses you.
    pub user_name: Option<String>,
    pub log_level: Option<String>,
    pub llm: LlmSettings,
    pub stt: SttSettings,
    pub tts: TtsSettings,
    pub avatar: AvatarSettings,
    pub memory: MemorySettings,
    pub web_search: WebSearchSettings,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebSearchSettings {
    pub enabled: Option<bool>,
    /// `duckduckgo` (no setup) or `searxng` (self-hosted).
    pub provider: Option<String>,
    /// Base URL of your SearXNG instance.
    pub searxng_url: Option<String>,
    /// `auto` (only when a message needs fresh info) or `always`.
    pub mode: Option<String>,
    pub max_results: Option<u64>,
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LlmSettings {
    pub enabled: Option<bool>,
    /// `ollama` or `openai` (any OpenAI-compatible server).
    pub provider: Option<String>,
    /// Keep the tiny built-in brain as the last resort (default true).
    pub fallback_brain: Option<bool>,
    /// Model name for the chosen provider. For Ollama `auto` = first installed of `model_candidates`.
    pub model: Option<String>,
    pub model_candidates: Option<Vec<String>>,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u64>,
    pub ollama_url: Option<String>,
    pub embed_model: Option<String>,
    pub openai_base_url: Option<String>,
    /// Name of the environment variable that holds the API key.
    pub openai_api_key_env: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SttSettings {
    pub enabled: Option<bool>,
    /// File in the models dir, e.g. `ggml-small.en.bin`; `auto` = what scripts/download-models picked.
    pub model: Option<String>,
    pub language: Option<String>,
    pub threads: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TtsSettings {
    pub enabled: Option<bool>,
    /// A key of `tts_engines` in services.yaml: `chatterbox`, `f5` or `piper`.
    pub engine: Option<String>,
    /// Voice folder name in the voices dir. Empty/absent = the persona's own `voice`.
    pub voice: Option<String>,
    /// `auto`, `cpu`, `cuda` or `mps` (Chatterbox / F5).
    pub device: Option<String>,
    /// Python interpreter for Python engines (`python`, `python3`, a venv path...).
    pub python: Option<String>,
    pub speed: Option<f64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AvatarSettings {
    /// `.vrm` file in the avatars dir. Empty/absent = the persona's own `avatar`.
    pub file: Option<String>,
    pub vtube_studio: VtsSettings,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VtsSettings {
    pub enabled: Option<bool>,
    pub url: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MemorySettings {
    pub enabled: Option<bool>,
    /// `hash` (local, no download) or `llm` (semantic, via Ollama embeddings).
    pub embedder: Option<String>,
    pub summarize_after: Option<u64>,
    pub keep_recent: Option<u64>,
    pub extract_with_llm: Option<bool>,
}

impl UserSettings {
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        // an empty file (or one that is only comments) parses as null
        let v: serde_yaml::Value = serde_yaml::from_str(text).context("invalid YAML")?;
        if v.is_null() {
            return Ok(Self::default());
        }
        serde_yaml::from_value(v).context("invalid settings")
    }
}

fn nonblank(s: &Option<String>) -> Option<&str> {
    s.as_deref().map(str::trim).filter(|v| !v.is_empty())
}

/// Set `root[path[0]][path[1]]... = val`, creating missing mappings on the way.
fn set_path(root: &mut serde_yaml::Value, path: &[&str], val: serde_yaml::Value) {
    use serde_yaml::{Mapping, Value};
    let mut cur = root;
    for key in path {
        if !cur.is_mapping() {
            *cur = Value::Mapping(Mapping::new());
        }
        let Value::Mapping(m) = cur else { unreachable!() };
        cur = m.entry(Value::String((*key).to_string())).or_insert(Value::Null);
    }
    *cur = val;
}

fn yv<T: serde::Serialize>(v: T) -> serde_yaml::Value {
    serde_yaml::to_value(v).unwrap_or(serde_yaml::Value::Null)
}

impl Config {
    /// Lay the user's settings over the services file. Only fields that are present change anything.
    pub fn apply_settings(&mut self, u: &UserSettings) {
        if let Some(l) = nonblank(&u.log_level) {
            self.global.log_level = l.to_string();
        }
        if let Some(name) = nonblank(&u.persona) {
            let file = if name.ends_with(".yaml") || name.ends_with(".yml") || name.contains('/') || name.contains('\\') {
                name.to_string()
            } else {
                format!("{}/{}.yaml", self.global.personas_dir.trim_end_matches('/'), name)
            };
            self.set_service_setting("persona", &["persona_file"], yv(file));
        }
        if let Some(n) = u.user_name.as_deref() {
            self.set_service_setting("orchestrator", &["user_name"], yv(n.trim()));
        }
        self.apply_web_search(&u.web_search);
        self.apply_llm(&u.llm);
        self.apply_stt(&u.stt);
        self.apply_tts(&u.tts);

        // voice / avatar overrides are applied by persona-service on top of the persona file
        if let Some(v) = nonblank(&u.tts.voice) {
            self.set_service_setting("persona", &["voice_override"], yv(v));
        }
        if let Some(a) = nonblank(&u.avatar.file) {
            self.set_service_setting("persona", &["avatar_override"], yv(a));
        }
        let vts = &u.avatar.vtube_studio;
        if let Some(b) = vts.enabled {
            self.set_service_setting("avatar", &["vtube_studio", "enabled"], yv(b));
        }
        if let Some(url) = nonblank(&vts.url) {
            self.set_service_setting("avatar", &["vtube_studio", "url"], yv(url));
        }

        let m = &u.memory;
        if let Some(b) = m.enabled {
            self.set_enabled("memory", b);
        }
        if let Some(e) = nonblank(&m.embedder) {
            self.set_service_setting("memory", &["embedder"], yv(e));
        }
        if let Some(n) = m.summarize_after {
            self.set_service_setting("memory", &["summarize_after"], yv(n));
        }
        if let Some(n) = m.keep_recent {
            self.set_service_setting("memory", &["keep_recent"], yv(n));
        }
        if let Some(b) = m.extract_with_llm {
            self.set_service_setting("memory", &["extract_with_llm"], yv(b));
        }
    }

    fn set_enabled(&mut self, role: &str, on: bool) {
        if let Some(s) = self.services.get_mut(role) {
            s.enabled = on;
        }
    }

    fn set_service_setting(&mut self, role: &str, path: &[&str], val: serde_yaml::Value) {
        if let Some(s) = self.services.get_mut(role) {
            set_path(&mut s.settings, path, val);
        }
    }

    fn apply_web_search(&mut self, w: &WebSearchSettings) {
        let p = |this: &mut Self, key: &str, v: serde_yaml::Value| this.set_service_setting("orchestrator", &["web_search", key], v);
        if let Some(b) = w.enabled {
            p(self, "enabled", yv(b));
        }
        if let Some(s) = nonblank(&w.provider) {
            p(self, "provider", yv(s.to_lowercase()));
        }
        if let Some(s) = nonblank(&w.searxng_url) {
            p(self, "searxng_url", yv(s));
        }
        if let Some(s) = nonblank(&w.mode) {
            p(self, "mode", yv(s.to_lowercase()));
        }
        if let Some(n) = w.max_results {
            p(self, "max_results", yv(n));
        }
        if let Some(n) = w.timeout_ms {
            p(self, "timeout_ms", yv(n));
        }
    }

    fn apply_llm(&mut self, l: &LlmSettings) {
        if let Some(b) = l.enabled {
            self.set_enabled("llm", b);
        }
        let provider = nonblank(&l.provider).map(|p| p.to_lowercase());
        if let Some(p) = &provider {
            let mut chain = vec![p.clone()];
            if l.fallback_brain.unwrap_or(true) && p != "fallback" {
                chain.push("fallback".into());
            }
            self.set_service_setting("llm", &["providers"], yv(chain));
        }
        if let Some(m) = nonblank(&l.model) {
            // `provider` decides which block gets the model; without it, look at what is configured now
            let openai_first = match &provider {
                Some(p) => p == "openai",
                None => {
                    self.services.get("llm").and_then(|s| s.settings.get("providers")).and_then(|p| p.get(0)).and_then(|p| p.as_str())
                        == Some("openai")
                }
            };
            let key = if openai_first { "openai" } else { "ollama" };
            self.set_service_setting("llm", &[key, "model"], yv(m));
        }
        if let Some(c) = &l.model_candidates {
            self.set_service_setting("llm", &["ollama", "model_candidates"], yv(c));
        }
        if let Some(t) = l.temperature {
            self.set_service_setting("llm", &["temperature"], yv(t));
        }
        if let Some(n) = l.max_tokens {
            self.set_service_setting("llm", &["max_tokens"], yv(n));
        }
        if let Some(u) = nonblank(&l.ollama_url) {
            self.set_service_setting("llm", &["ollama", "base_url"], yv(u));
        }
        if let Some(m) = nonblank(&l.embed_model) {
            self.set_service_setting("llm", &["ollama", "embed_model"], yv(m));
        }
        if let Some(u) = nonblank(&l.openai_base_url) {
            self.set_service_setting("llm", &["openai", "base_url"], yv(u));
        }
        if let Some(k) = nonblank(&l.openai_api_key_env) {
            self.set_service_setting("llm", &["openai", "api_key_env"], yv(k));
        }
    }

    fn apply_stt(&mut self, s: &SttSettings) {
        if let Some(b) = s.enabled {
            self.set_enabled("stt", b);
        }
        if let Some(m) = nonblank(&s.model) {
            self.set_service_setting("stt", &["model"], yv(m));
        }
        if let Some(l) = nonblank(&s.language) {
            self.set_service_setting("stt", &["language"], yv(l));
        }
        if let Some(n) = s.threads {
            self.set_service_setting("stt", &["threads"], yv(n));
        }
    }

    fn apply_tts(&mut self, t: &TtsSettings) {
        if let Some(b) = t.enabled {
            self.set_enabled("tts", b);
        }
        if let Some(name) = nonblank(&t.engine) {
            match self.tts_engines.get(name).cloned() {
                Some(e) => {
                    if let Some(svc) = self.services.get_mut("tts") {
                        let builtin = e.implementation.as_deref().unwrap_or("builtin") == "builtin";
                        svc.implementation = if builtin { "builtin".into() } else { "external".into() };
                        svc.name = e.name.or_else(|| Some(name.to_string()));
                        svc.bin = e.bin;
                        svc.command =
                            if builtin { None } else { e.command.map(|c| c.into_iter().map(|a| a.replace("{addr}", &svc.addr)).collect()) };
                        if let Some(n) = e.startup_timeout_s {
                            svc.startup_timeout_s = n;
                        }
                        svc.env.extend(e.env);
                    }
                }
                None => {
                    let known: Vec<&str> = self.tts_engines.keys().map(String::as_str).collect();
                    tracing::warn!(engine = name, ?known, "unknown tts.engine in settings.yaml, keeping services.yaml");
                }
            }
        }
        if let Some(svc) = self.services.get_mut("tts") {
            if let (Some(py), Some(cmd)) = (nonblank(&t.python), svc.command.as_mut()) {
                if let Some(first) = cmd.first_mut() {
                    if first == "python" || first == "python3" {
                        *first = py.to_string();
                    }
                }
            }
            if let Some(d) = nonblank(&t.device) {
                svc.env.insert("WAIFU_TTS_DEVICE".into(), d.to_string());
            }
        }
        if let Some(sp) = t.speed {
            self.set_service_setting("tts", &["speed"], yv(sp));
        }
    }
}

#[cfg(test)]
mod settings_tests {
    use super::*;

    const SERVICES: &str = include_str!("../../../../config/services.yaml");
    const SHIPPED: &str = include_str!("../../../../config/settings.yaml");

    fn base() -> Config {
        Config::parse(SERVICES).unwrap()
    }
    fn s(cfg: &Config, role: &str, path: &[&str]) -> Option<serde_yaml::Value> {
        let mut v = &cfg.services[role].settings;
        for k in path {
            v = v.get(*k)?;
        }
        Some(v.clone())
    }
    fn apply(yaml: &str) -> Config {
        let mut c = base();
        c.apply_settings(&UserSettings::parse(yaml).unwrap());
        c
    }

    #[test]
    fn shipped_settings_parse_and_apply() {
        let mut c = base();
        c.apply_settings(&UserSettings::parse(SHIPPED).unwrap());
        assert_eq!(s(&c, "persona", &["persona_file"]).unwrap(), "config/personas/mio.yaml");
        assert_eq!(s(&c, "llm", &["ollama", "model"]).unwrap(), "gemma3:4b");
        assert_eq!(s(&c, "llm", &["providers"]).unwrap(), serde_yaml::from_str::<serde_yaml::Value>("[ollama, fallback]").unwrap());
        assert_eq!(c.services["tts"].name.as_deref(), Some("chatterbox-turbo"));
        assert!(c.services["tts"].command.as_ref().unwrap().iter().any(|a| a == "127.0.0.1:60001"));
        // blank voice / avatar must not create overrides
        assert!(s(&c, "persona", &["voice_override"]).is_none());
        assert!(s(&c, "persona", &["avatar_override"]).is_none());
    }

    #[test]
    fn empty_or_comment_only_file_changes_nothing() {
        let c = apply("# nothing here\n");
        assert_eq!(s(&c, "persona", &["persona_file"]).unwrap(), "config/personas/mio.yaml");
        assert!(UserSettings::parse("").is_ok());
    }

    #[test]
    fn typos_are_rejected() {
        assert!(UserSettings::parse("llm:\n  modle: x\n").is_err());
        assert!(UserSettings::parse("lmm: {}\n").is_err());
    }

    #[test]
    fn model_goes_to_the_chosen_provider() {
        let c = apply("llm:\n  provider: openai\n  model: gpt-x\n  fallback_brain: false\n");
        assert_eq!(s(&c, "llm", &["openai", "model"]).unwrap(), "gpt-x");
        assert_eq!(s(&c, "llm", &["ollama", "model"]).unwrap(), "auto");
        assert_eq!(s(&c, "llm", &["providers"]).unwrap(), serde_yaml::from_str::<serde_yaml::Value>("[openai]").unwrap());
        let c = apply("llm:\n  model: qwen2.5:7b\n"); // no provider: follows the configured chain (ollama first)
        assert_eq!(s(&c, "llm", &["ollama", "model"]).unwrap(), "qwen2.5:7b");
    }

    #[test]
    fn persona_by_name_or_path() {
        assert_eq!(s(&apply("persona: aria-sage\n"), "persona", &["persona_file"]).unwrap(), "config/personas/aria-sage.yaml");
        assert_eq!(s(&apply("persona: my/own.yaml\n"), "persona", &["persona_file"]).unwrap(), "my/own.yaml");
    }

    #[test]
    fn voice_and_avatar_become_persona_overrides() {
        let c = apply("tts:\n  voice: ref\navatar:\n  file: x.vrm\n");
        assert_eq!(s(&c, "persona", &["voice_override"]).unwrap(), "ref");
        assert_eq!(s(&c, "persona", &["avatar_override"]).unwrap(), "x.vrm");
    }

    #[test]
    fn tts_engine_switch_rewrites_the_service_entry() {
        let c = apply("tts:\n  engine: piper\n");
        let t = &c.services["tts"];
        assert_eq!(t.implementation, "builtin");
        assert!(t.command.is_none());
        assert_eq!(t.bin.as_deref(), Some("tts-service"));

        let c = apply("tts:\n  engine: f5\n  python: /venv/bin/python\n  device: mps\n");
        let t = &c.services["tts"];
        assert_eq!(t.implementation, "external");
        let cmd = t.command.as_ref().unwrap();
        assert_eq!(cmd[0], "/venv/bin/python");
        assert!(cmd[1].ends_with("f5_tts_service.py") && cmd.last().unwrap() == &t.addr);
        assert_eq!(t.env.get("WAIFU_TTS_DEVICE").map(String::as_str), Some("mps"));

        // unknown engine: warn and keep what services.yaml says
        let c = apply("tts:\n  engine: nope\n");
        assert_eq!(c.services["tts"].name.as_deref(), Some("chatterbox-turbo"));
    }

    #[test]
    fn enabled_flags() {
        let c = apply("stt: {enabled: false}\ntts: {enabled: false}\nmemory: {enabled: false}\n");
        assert!(!c.services["stt"].enabled && !c.services["tts"].enabled && !c.services["memory"].enabled);
        assert!(c.services["llm"].enabled);
    }
}
