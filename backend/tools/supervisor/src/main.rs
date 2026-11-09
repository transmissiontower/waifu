//! supervisor: starts every builtin service in dependency order, waits for health, restarts crashed
//! services with exponential backoff, aggregates logs and shuts everything down gracefully.
//!
//! usage: supervisor [--config config/services.yaml] [--root DIR] [--open] [--only a,b] [--skip x,y] [--bin-dir DIR]
//!                   [--sequential] [--status-dir DIR]
//!
//! Services may print `@@loading <what>` / `@@ready` / `@@failed <why>` lines on stdout while they warm up; they are
//! shown on the loading screen (status file `<data_dir>/boot/services.json`) and start-up waits for them.
use anyhow::{anyhow, Context};
use common::{Config, ServiceCfg};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

struct Opts {
    config: PathBuf,
    /// project root: services run with this as their working directory (default: the current directory)
    root: Option<PathBuf>,
    open: bool,
    only: Option<BTreeSet<String>>,
    skip: BTreeSet<String>,
    bin_dir: PathBuf,
    /// start in dependency waves (old behaviour) instead of everything at once
    sequential: bool,
    status_dir: Option<PathBuf>,
}

fn parse_args() -> anyhow::Result<Opts> {
    let mut o = Opts {
        config: PathBuf::from("config/services.yaml"),
        root: None,
        open: false,
        only: None,
        skip: BTreeSet::new(),
        bin_dir: std::env::current_exe()?.parent().map(|p| p.to_path_buf()).unwrap_or_default(),
        sequential: false,
        status_dir: None,
    };
    let csv = |s: String| s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect::<BTreeSet<_>>();
    let mut a = std::env::args().skip(1);
    while let Some(x) = a.next() {
        match x.as_str() {
            "--config" => o.config = a.next().context("--config needs a path")?.into(),
            "--root" => o.root = Some(a.next().context("--root needs a path")?.into()),
            "--open" => o.open = true,
            "--only" => o.only = Some(csv(a.next().context("--only needs a list")?)),
            "--skip" => o.skip = csv(a.next().context("--skip needs a list")?),
            "--bin-dir" => o.bin_dir = a.next().context("--bin-dir needs a path")?.into(),
            "--sequential" => o.sequential = true,
            "--status-dir" => o.status_dir = Some(a.next().context("--status-dir needs a path")?.into()),
            "-h" | "--help" => {
                println!("supervisor [--config config/services.yaml] [--root DIR] [--open] [--only a,b] [--skip x,y] [--bin-dir DIR] [--sequential] [--status-dir DIR]");
                std::process::exit(0);
            }
            other => return Err(anyhow!("unknown argument {other}")),
        }
    }
    Ok(o)
}

fn default_bin(role: &str) -> String {
    match role {
        "gateway" | "orchestrator" => role.to_string(),
        r => format!("{r}-service"),
    }
}

/// Group enabled services into start waves by `depends_on` (dependencies on disabled/unknown services are ignored).
pub fn waves(services: &BTreeMap<String, ServiceCfg>, active: &BTreeSet<String>) -> Vec<Vec<String>> {
    let mut done: BTreeSet<String> = BTreeSet::new();
    let mut left: BTreeSet<String> = active.clone();
    let mut out = vec![];
    while !left.is_empty() {
        let wave: Vec<String> =
            left.iter().filter(|r| services[*r].depends_on.iter().all(|d| done.contains(d) || !active.contains(d))).cloned().collect();
        if wave.is_empty() {
            // cycle: start the rest together rather than deadlock
            out.push(left.iter().cloned().collect());
            break;
        }
        for w in &wave {
            left.remove(w);
            done.insert(w.clone());
        }
        out.push(wave);
    }
    out
}

fn label(role: &str) -> &'static str {
    match role {
        "llm" => "Language model",
        "stt" => "Speech recognition",
        "tts" => "Voice",
        "memory" => "Memory",
        "persona" => "Personality",
        "avatar" => "Avatar hub",
        "orchestrator" => "Conversation engine",
        "gateway" => "Web gateway",
        _ => "Service",
    }
}

fn jstr(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// What the loading screen shows for one service.
#[derive(Clone, Default)]
struct Entry {
    label: String,
    detail: String,
    healthy: bool,
    loading: bool,
    degraded: bool,
    failed: bool,
    restarting: bool,
}

impl Entry {
    fn state(&self) -> &'static str {
        if self.failed {
            "failed"
        } else if self.restarting {
            "restarting"
        } else if self.degraded {
            "degraded"
        } else if self.loading {
            "loading"
        } else if self.healthy {
            "ready"
        } else {
            "starting"
        }
    }
}

struct Board {
    path: Option<PathBuf>,
    url: String,
    overall: &'static str,
    services: BTreeMap<String, Entry>,
}

type SharedBoard = Arc<Mutex<Board>>;

impl Board {
    fn json(&self) -> String {
        let items: Vec<String> = self
            .services
            .iter()
            .map(|(role, e)| {
                format!(
                    "{{\"role\":{},\"label\":{},\"state\":{},\"detail\":{}}}",
                    jstr(role),
                    jstr(&e.label),
                    jstr(e.state()),
                    jstr(&e.detail)
                )
            })
            .collect();
        let ms = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
        format!("{{\"overall\":{},\"url\":{},\"updated\":{},\"services\":[{}]}}", jstr(self.overall), jstr(&self.url), ms, items.join(","))
    }
    /// atomic replace, so a reader never sees half a file
    fn flush(&self) {
        let Some(p) = &self.path else { return };
        let tmp = p.with_extension("json.tmp");
        if std::fs::write(&tmp, self.json()).is_ok() {
            let _ = std::fs::rename(&tmp, p);
        }
    }
}

fn update(board: &SharedBoard, role: &str, f: impl FnOnce(&mut Entry)) {
    let mut b = board.lock().unwrap();
    if let Some(e) = b.services.get_mut(role) {
        f(e);
    }
    b.flush();
}

fn is_loading(board: &SharedBoard, role: &str) -> bool {
    board.lock().unwrap().services.get(role).map(|e| e.loading).unwrap_or(false)
}

/// `@@kind detail` -> (kind, detail)
fn marker(line: &str) -> Option<(&str, &str)> {
    let rest = line.strip_prefix("@@")?;
    let mut it = rest.splitn(2, ' ');
    let kind = it.next()?;
    Some((kind, it.next().unwrap_or("").trim()))
}

fn color(role: &str) -> &'static str {
    const C: [&str; 6] = ["\x1b[36m", "\x1b[32m", "\x1b[33m", "\x1b[35m", "\x1b[34m", "\x1b[91m"];
    C[role.bytes().fold(0usize, |a, b| a.wrapping_add(b as usize)) % C.len()]
}

fn sup(msg: &str) {
    println!("\x1b[1m[supervisor]\x1b[0m {msg}");
}

async fn pipe<R: tokio::io::AsyncRead + Unpin>(role: String, r: R, board: SharedBoard) {
    let mut lines = BufReader::new(r).lines();
    while let Ok(Some(l)) = lines.next_line().await {
        if let Some((kind, detail)) = marker(&l) {
            match kind {
                "loading" => update(&board, &role, |e| {
                    e.loading = true;
                    e.detail = detail.to_string();
                }),
                "ready" => update(&board, &role, |e| {
                    e.loading = false;
                    e.detail.clear();
                }),
                "failed" => update(&board, &role, |e| {
                    e.loading = false;
                    e.degraded = true;
                    e.detail = detail.to_string();
                }),
                _ => {}
            }
            println!("{}[{role}]\x1b[0m \u{25b8} {kind} {detail}", color(&role));
            continue;
        }
        println!("{}[{role}]\x1b[0m {l}", color(&role));
    }
}

async fn healthy(http: &reqwest::Client, svc: &ServiceCfg) -> bool {
    if let (Some(h), "builtin") = (&svc.http, svc.implementation.as_str()) {
        matches!(http.get(format!("http://{h}/healthz")).send().await, Ok(r) if r.status().is_success())
    } else {
        matches!(tokio::time::timeout(Duration::from_millis(500), tokio::net::TcpStream::connect(&svc.addr)).await, Ok(Ok(_)))
    }
}

fn build_command(role: &str, svc: &ServiceCfg, o: &Opts, cfg_abs: &PathBuf, root: &PathBuf) -> anyhow::Result<Command> {
    let mut cmd = if svc.implementation == "builtin" {
        let name = svc.bin.clone().unwrap_or_else(|| default_bin(role));
        let mut p = o.bin_dir.join(&name);
        if cfg!(windows) {
            p.set_extension("exe");
        }
        anyhow::ensure!(p.exists(), "binary {} not found (run `cargo build --release`)", p.display());
        let mut c = Command::new(p);
        c.env("WAIFU_SERVICE", role);
        c
    } else {
        let argv = svc.command.clone().ok_or_else(|| anyhow!("external service '{role}' has no `command`"))?;
        let mut c = Command::new(&argv[0]);
        c.args(&argv[1..]);
        c
    };
    cmd.current_dir(root).env("WAIFU_CONFIG", cfg_abs).env("WAIFU_ROLE", role).env("WAIFU_ADDR", &svc.addr);
    if let Some(h) = &svc.http {
        cmd.env("WAIFU_HTTP", h);
    }
    // Asset folders from the config, as absolute paths, for services that do not read the config themselves (Python engines).
    // Config::load() also re-reads settings.yaml, so a restarted service picks up edits.
    let global = Config::load().map(|c| c.global).unwrap_or_default();
    let abs = |d: &str| {
        let p = PathBuf::from(d);
        if p.is_absolute() {
            p
        } else {
            root.join(p)
        }
    };
    cmd.env("WAIFU_MODELS_DIR", abs(&global.models_dir))
        .env("WAIFU_VOICES_DIR", abs(&global.voices_dir))
        .env("WAIFU_AVATARS_DIR", abs(&global.avatars_dir));
    // `model: auto` = the pick scripts/download-models made for this hardware (<models_dir>/.stt-model, .llm-model).
    // An explicit model in settings.yaml / services.yaml wins over the file; an explicit WAIFU_* env var wins over both.
    let model_at = |path: &[&str]| -> Option<String> {
        let mut v = &svc.settings;
        for k in path {
            v = v.get(*k)?;
        }
        v.as_str().map(|s| s.trim().to_string())
    };
    let auto_pick: Option<(&str, &str, Option<&str>)> = match role {
        "stt" if model_at(&["model"]).as_deref() == Some("auto") => Some((".stt-model", "WAIFU_STT_MODEL", Some("ggml-base.en.bin"))),
        "llm" if matches!(model_at(&["ollama", "model"]).as_deref(), None | Some("auto") | Some("")) => {
            Some((".llm-model", "WAIFU_LLM_MODEL", None))
        }
        _ => None,
    };
    if let Some((file, var, default)) = auto_pick {
        if std::env::var(var).is_err() {
            let from_file =
                std::fs::read_to_string(abs(&global.models_dir).join(file)).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
            if let Some(v) = from_file.or_else(|| default.map(String::from)) {
                cmd.env(var, v);
            }
        }
    }
    for (k, v) in &svc.env {
        cmd.env(k, v);
    }
    // shared libraries next to the binaries (sherpa-onnx / onnxruntime)
    let libdirs = o.bin_dir.display().to_string();
    #[cfg(windows)]
    cmd.env("PATH", format!("{libdirs};{}", std::env::var("PATH").unwrap_or_default()));
    #[cfg(not(windows))]
    {
        cmd.env("LD_LIBRARY_PATH", format!("{libdirs}:{}", std::env::var("LD_LIBRARY_PATH").unwrap_or_default()));
        cmd.env("DYLD_LIBRARY_PATH", &libdirs);
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    Ok(cmd)
}

/// Ask the service to stop itself (cross-platform graceful path), then force-kill after a grace period.
async fn stop_child(role: &str, svc: &ServiceCfg, child: &mut Child, http: &reqwest::Client) {
    if let Some(h) = &svc.http {
        let _ = http.post(format!("http://{h}/admin/shutdown")).send().await;
    } else if role == "gateway" {
        let _ = http.post(format!("http://{}/admin/shutdown", svc.addr)).send().await;
    }
    match tokio::time::timeout(Duration::from_secs(6), child.wait()).await {
        Ok(_) => sup(&format!("{role} stopped gracefully")),
        Err(_) => {
            sup(&format!("{role} did not stop in time - killing"));
            let _ = child.kill().await;
        }
    }
}

async fn run_service(
    role: String,
    svc: ServiceCfg,
    o: Arc<Opts>,
    cfg_abs: PathBuf,
    root: PathBuf,
    ready: Arc<Notify>,
    shutdown: CancellationToken,
    board: SharedBoard,
) {
    let http = reqwest::Client::builder().timeout(Duration::from_secs(2)).build().unwrap();
    let mut backoff = Duration::from_secs(1);
    let mut first = true;
    loop {
        if shutdown.is_cancelled() {
            return;
        }
        let mut cmd = match build_command(&role, &svc, &o, &cfg_abs, &root) {
            Ok(c) => c,
            Err(e) => {
                sup(&format!("\x1b[31m{role}: cannot start: {e}\x1b[0m"));
                update(&board, &role, |x| {
                    x.failed = true;
                    x.detail = e.to_string();
                });
                ready.notify_one();
                return;
            }
        };
        let started = Instant::now();
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                sup(&format!("\x1b[31m{role}: spawn failed: {e}\x1b[0m"));
                update(&board, &role, |x| {
                    x.failed = true;
                    x.detail = e.to_string();
                });
                ready.notify_one();
                return;
            }
        };
        sup(&format!("{role} started (pid {}) {}", child.id().unwrap_or(0), if first { "" } else { "[restart]" }));
        let restarted = !first;
        first = false;
        update(&board, &role, |x| {
            x.healthy = false;
            x.loading = false;
            x.degraded = false;
            x.failed = false;
            x.restarting = restarted;
            x.detail.clear();
        });
        if let Some(out) = child.stdout.take() {
            tokio::spawn(pipe(role.clone(), out, board.clone()));
        }
        if let Some(err) = child.stderr.take() {
            tokio::spawn(pipe(role.clone(), err, board.clone()));
        }
        // wait for first healthy check
        let deadline = Instant::now() + Duration::from_secs(svc.startup_timeout_s);
        let mut up = false;
        let mut exited = false;
        while Instant::now() < deadline {
            tokio::select! {
                _ = shutdown.cancelled() => { stop_child(&role, &svc, &mut child, &http).await; return; }
                _ = child.wait() => { exited = true; break; }
                _ = tokio::time::sleep(Duration::from_millis(100)) => {}
            }
            if healthy(&http, &svc).await {
                up = true;
                break;
            }
        }
        if up {
            sup(&format!("\x1b[32m{role} is healthy\x1b[0m ({:.1}s)", started.elapsed().as_secs_f32()));
        } else if !exited {
            sup(&format!("\x1b[33m{role} not healthy after {}s - continuing anyway (degraded mode)\x1b[0m", svc.startup_timeout_s));
        }
        update(&board, &role, |x| {
            x.restarting = false;
            if up {
                x.healthy = true;
            } else if exited {
                x.failed = true;
                x.detail = "exited during start-up".into();
            } else {
                x.degraded = true;
                x.detail = format!("not healthy after {}s", svc.startup_timeout_s);
            }
        });
        if up {
            // a service that announces `@@loading` (model load / warm-up) is only "ready" once it says `@@ready`
            tokio::time::sleep(Duration::from_millis(200)).await;
            let mut died = false;
            while is_loading(&board, &role) && Instant::now() < deadline {
                tokio::select! {
                    _ = shutdown.cancelled() => { stop_child(&role, &svc, &mut child, &http).await; return; }
                    _ = child.wait() => { died = true; break; }
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {}
                }
            }
            if died {
                sup(&format!("\x1b[31m{role} exited while loading\x1b[0m"));
                update(&board, &role, |x| {
                    x.loading = false;
                    x.failed = true;
                    x.detail = "exited while loading".into();
                });
            } else if is_loading(&board, &role) {
                sup(&format!("\x1b[33m{role} still loading after {}s - continuing anyway\x1b[0m", svc.startup_timeout_s));
                update(&board, &role, |x| {
                    x.loading = false;
                    x.degraded = true;
                    x.detail = format!("still loading after {}s", svc.startup_timeout_s);
                });
            } else {
                sup(&format!("\x1b[32m{role} is ready\x1b[0m ({:.1}s)", started.elapsed().as_secs_f32()));
            }
        }
        ready.notify_one();
        // monitor
        let mut bad = 0u32;
        if !exited {
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => { stop_child(&role, &svc, &mut child, &http).await; return; }
                    st = child.wait() => { sup(&format!("\x1b[31m{role} exited: {:?}\x1b[0m", st.ok())); break; }
                    _ = tokio::time::sleep(Duration::from_secs(3)) => {
                        if healthy(&http, &svc).await { bad = 0; } else { bad += 1; }
                        if bad >= 5 {
                            sup(&format!("\x1b[31m{role} unresponsive - restarting\x1b[0m"));
                            let _ = child.kill().await;
                            break;
                        }
                    }
                }
            }
        }
        // restart with exponential backoff (reset after a long stable run)
        if started.elapsed() > Duration::from_secs(60) {
            backoff = Duration::from_secs(1);
        }
        sup(&format!("{role} restarting in {:?}", backoff));
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = tokio::time::sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

fn open_window(url: &str) {
    let url = url.to_string();
    let _ = std::thread::spawn(move || {
        let run = |prog: &str, args: &[&str]| {
            std::process::Command::new(prog).args(args).stdout(Stdio::null()).stderr(Stdio::null()).spawn().is_ok()
        };
        let app = format!("--app={url}");
        #[cfg(windows)]
        {
            let _ = run("cmd", &["/C", "start", "", "msedge", &app, "--window-size=520,820"])
                || run("cmd", &["/C", "start", "", "chrome", &app, "--window-size=520,820"])
                || run("cmd", &["/C", "start", "", &url]);
        }
        #[cfg(target_os = "macos")]
        {
            let _ = run("open", &["-na", "Google Chrome", "--args", &app, "--window-size=520,820"]) || run("open", &[&url]);
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            let _ = ["google-chrome", "chromium", "chromium-browser", "microsoft-edge"]
                .iter()
                .any(|b| run(b, &[&app, "--window-size=520,820"]))
                || run("xdg-open", &[&url]);
        }
    });
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let o = Arc::new(parse_args()?);
    let cfg_abs = std::fs::canonicalize(&o.config).with_context(|| format!("config {} not found", o.config.display()))?;
    let root = match &o.root {
        Some(r) => std::fs::canonicalize(r).with_context(|| format!("root {} not found", r.display()))?,
        None => std::env::current_dir()?,
    };
    std::env::set_var("WAIFU_CONFIG", &cfg_abs);
    let cfg = Config::load()?;
    let active: BTreeSet<String> = cfg
        .services
        .iter()
        .filter(|(r, s)| s.enabled && !o.skip.contains(*r) && o.only.as_ref().map(|x| x.contains(*r)).unwrap_or(true))
        // external services without a command are only health-probed, never launched
        .filter(|(_, s)| s.implementation == "builtin" || s.command.is_some())
        .map(|(r, _)| r.clone())
        .collect();
    for (r, s) in &cfg.services {
        if s.enabled && s.implementation != "builtin" && s.command.is_none() {
            sup(&format!("{r}: external implementation at {} (not supervised)", s.addr));
        }
    }
    let shutdown = CancellationToken::new();
    {
        let t = shutdown.clone();
        tokio::spawn(async move {
            common::telemetry::wait_for_signal().await;
            t.cancel();
        });
    }
    // Channels are lazy and every call has a deadline + breaker, so start order does not matter: starting everything
    // at once makes boot as fast as the slowest service instead of the sum of all waves.
    let plan = if o.sequential { waves(&cfg.services, &active) } else { vec![active.iter().cloned().collect::<Vec<String>>()] };
    sup(&format!("start plan: {}", plan.iter().map(|w| format!("[{}]", w.join(", "))).collect::<Vec<_>>().join(" -> ")));
    // loading-screen status file (<data_dir>/boot/services.json); failure to write it is never fatal
    let status_dir = o.status_dir.clone().unwrap_or_else(|| root.join(&cfg.global.data_dir).join("boot"));
    let status_path = std::fs::create_dir_all(&status_dir).ok().map(|_| status_dir.join("services.json"));
    let board: SharedBoard = Arc::new(Mutex::new(Board {
        path: status_path,
        url: cfg.services.get("gateway").map(|g| format!("http://{}", g.addr)).unwrap_or_default(),
        overall: "starting",
        services: active.iter().map(|r| (r.clone(), Entry { label: label(r).to_string(), ..Default::default() })).collect(),
    }));
    board.lock().unwrap().flush();
    let mut handles = vec![];
    for wave in plan {
        let mut readies = vec![];
        for role in wave {
            let ready = Arc::new(Notify::new());
            readies.push(ready.clone());
            handles.push(tokio::spawn(run_service(
                role.clone(),
                cfg.services[&role].clone(),
                o.clone(),
                cfg_abs.clone(),
                root.clone(),
                ready,
                shutdown.clone(),
                board.clone(),
            )));
        }
        for r in readies {
            tokio::select! { _ = r.notified() => {}, _ = shutdown.cancelled() => break }
        }
        if shutdown.is_cancelled() {
            break;
        }
    }
    if !shutdown.is_cancelled() {
        {
            let mut b = board.lock().unwrap();
            b.overall = "ready";
            b.flush();
        }
        let gw = &cfg.services["gateway"].addr;
        sup(&format!("\x1b[1;32mall services started - open http://{gw}\x1b[0m   (Ctrl+C to stop)"));
        if o.open && active.contains("gateway") {
            open_window(&format!("http://{gw}"));
        }
    }
    shutdown.cancelled().await;
    sup("shutting down...");
    {
        let mut b = board.lock().unwrap();
        b.overall = "stopped";
        b.flush();
    }
    for h in handles {
        let _ = h.await;
    }
    sup("bye");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(yaml: &str) -> Config {
        Config::parse(yaml).unwrap()
    }

    #[test]
    fn waves_follow_dependencies() {
        let c = cfg("services:\n a: {addr: x:1}\n b: {addr: x:2, depends_on: [a]}\n c: {addr: x:3, depends_on: [a, b]}\n d: {addr: x:4}\n");
        let active: BTreeSet<String> = c.services.keys().cloned().collect();
        assert_eq!(waves(&c.services, &active), vec![vec!["a".to_string(), "d".into()], vec!["b".into()], vec!["c".into()]]);
    }

    #[test]
    fn disabled_dependencies_are_ignored_and_cycles_do_not_deadlock() {
        let c = cfg("services:\n a: {addr: x:1, depends_on: [zzz]}\n b: {addr: x:2, depends_on: [c]}\n c: {addr: x:3, depends_on: [b]}\n");
        let active: BTreeSet<String> = c.services.keys().cloned().collect();
        let w = waves(&c.services, &active);
        assert_eq!(w[0], vec!["a".to_string()]);
        assert_eq!(w[1].len(), 2);
    }

    #[test]
    fn status_markers_are_parsed() {
        assert_eq!(marker("@@loading warming up the model"), Some(("loading", "warming up the model")));
        assert_eq!(marker("@@ready"), Some(("ready", "")));
        assert_eq!(marker("INFO plain log line"), None);
    }

    #[test]
    fn status_json_is_escaped_and_reflects_state() {
        assert_eq!(jstr("a\"b\\c\n"), "\"a\\\"b\\\\c\\n\"");
        let mut services = BTreeMap::new();
        services.insert(
            "tts".to_string(),
            Entry { label: "Voice".into(), detail: "say \"hi\"".into(), healthy: true, loading: true, ..Default::default() },
        );
        let b = Board { path: None, url: "http://127.0.0.1:8080".into(), overall: "starting", services };
        let j = b.json();
        assert!(j.contains("\"overall\":\"starting\""), "{j}");
        assert!(j.contains("\"state\":\"loading\""), "{j}");
        assert!(j.contains("say \\\"hi\\\""), "{j}");
    }

    #[test]
    fn shipped_config_parses_and_has_all_roles() {
        let c = cfg(&std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../../config/services.yaml")).unwrap());
        for r in ["gateway", "orchestrator", "llm", "stt", "tts", "memory", "persona", "avatar"] {
            assert!(c.services.contains_key(r), "{r}");
        }
        let active: BTreeSet<String> = c.services.keys().cloned().collect();
        let w = waves(&c.services, &active);
        assert_eq!(w.last().unwrap(), &vec!["gateway".to_string()]);
        assert!(w.iter().position(|x| x.contains(&"orchestrator".to_string())) > w.iter().position(|x| x.contains(&"memory".to_string())));
    }
}
