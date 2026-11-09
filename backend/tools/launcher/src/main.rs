//! launcher: a tiny, dependency-free loading-screen server.
//!
//! It is the first thing `run.sh` / `run.bat` start (it compiles in about a second) and it stays alive while the
//! rest of the app compiles, downloads models and starts, so the user sees what is going on instead of a blank
//! terminal. It only reads files from `--dir` (default `data/boot`):
//!   phase.txt      `phase=build|models|frontend|services|error`, `label=...`, `detail=...`, `error=...` (written by the run scripts)
//!   build.log      raw `cargo build` output (crates compiled so far are counted from it)
//!   services.json  per-service state written by the supervisor (`overall` becomes `ready` when everything is up)
//! The page polls `/status`; when the supervisor says `ready` it navigates to the app (`url` in services.json).
//!
//! usage: launcher [--port 8079] [--dir data/boot] [--root .] [--open]
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

const SPLASH: &str = include_str!("splash.html");

struct Opts {
    port: u16,
    dir: PathBuf,
    root: PathBuf,
    open: bool,
}

fn parse_args() -> Result<Opts, String> {
    let mut o = Opts { port: 8079, dir: PathBuf::from("data/boot"), root: PathBuf::from("."), open: false };
    let mut a = std::env::args().skip(1);
    while let Some(x) = a.next() {
        match x.as_str() {
            "--port" => o.port = a.next().ok_or("--port needs a number")?.parse().map_err(|_| "bad --port".to_string())?,
            "--dir" => o.dir = a.next().ok_or("--dir needs a path")?.into(),
            "--root" => o.root = a.next().ok_or("--root needs a path")?.into(),
            "--open" => o.open = true,
            "-h" | "--help" => {
                println!("launcher [--port 8079] [--dir data/boot] [--root .] [--open]");
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(o)
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

fn read_file(p: &Path) -> String {
    fs::read(p).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default()
}

fn key_values(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|l| {
            let (k, v) = l.split_once('=')?;
            Some((k.trim().to_string(), v.trim().to_string()))
        })
        .collect()
}

fn get<'a>(kv: &'a [(String, String)], key: &str) -> &'a str {
    kv.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str()).unwrap_or("")
}

/// (crates compiled, crates downloaded, crate being compiled, last few log lines)
fn build_stats(log: &str) -> (usize, usize, String, Vec<String>) {
    let (mut compiled, mut downloaded) = (0usize, 0usize);
    let mut current = String::new();
    let mut tail: Vec<String> = Vec::new();
    for line in log.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        if let Some(rest) = t.strip_prefix("Compiling ") {
            compiled += 1;
            current = rest.split_whitespace().next().unwrap_or("").to_string();
        } else if t.starts_with("Downloaded ") {
            downloaded += 1;
        }
        tail.push(t.chars().take(160).collect());
        if tail.len() > 8 {
            tail.remove(0);
        }
    }
    (compiled, downloaded, current, tail)
}

/// The persona file in use: `persona:` in config/settings.yaml (a name or a path), default `mio`.
fn persona_path(root: &Path) -> std::path::PathBuf {
    let chosen = fs::read_to_string(root.join("config/settings.yaml")).ok().and_then(|t| {
        t.lines().find_map(|l| {
            let v = l.strip_prefix("persona:")?; // top-level key only (no indentation)
            let v = v.split('#').next().unwrap_or("").trim().trim_matches('"').trim_matches('\'').to_string();
            (!v.is_empty()).then_some(v)
        })
    });
    match chosen {
        Some(v) if v.ends_with(".yaml") || v.ends_with(".yml") || v.contains('/') => root.join(v),
        Some(v) => root.join("config/personas").join(format!("{v}.yaml")),
        None => root.join("config/personas/mio.yaml"),
    }
}

fn persona_name(root: &Path) -> String {
    fs::read_to_string(persona_path(root))
        .ok()
        .and_then(|t| t.lines().find_map(|l| l.strip_prefix("name:").map(|v| v.trim().trim_matches('"').trim_matches('\'').to_string())))
        .unwrap_or_default()
}

fn status_json(o: &Opts) -> String {
    let ph = key_values(&read_file(&o.dir.join("phase.txt")));
    let (compiled, downloaded, current, tail) = build_stats(&read_file(&o.dir.join("build.log")));
    let services = read_file(&o.dir.join("services.json"));
    let services = if services.trim_start().starts_with('{') { services } else { "null".to_string() };
    format!(
        "{{\"name\":{},\"phase\":{},\"label\":{},\"detail\":{},\"error\":{},\"compiled\":{},\"downloaded\":{},\"crate\":{},\"tail\":[{}],\"services\":{}}}",
        jstr(&persona_name(&o.root)),
        jstr(get(&ph, "phase")),
        jstr(get(&ph, "label")),
        jstr(get(&ph, "detail")),
        jstr(get(&ph, "error")),
        compiled,
        downloaded,
        jstr(&current),
        tail.iter().map(|l| jstr(l)).collect::<Vec<_>>().join(","),
        services.trim()
    )
}

fn respond(mut s: TcpStream, code: &str, ctype: &str, body: &[u8]) {
    let head = format!(
        "HTTP/1.1 {code}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = s.write_all(head.as_bytes());
    let _ = s.write_all(body);
}

fn handle(mut s: TcpStream, o: &Opts) {
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    let mut buf = [0u8; 4096];
    let n = s.read(&mut buf).unwrap_or(0);
    let req = String::from_utf8_lossy(&buf[..n]);
    let path = req.lines().next().and_then(|l| l.split_whitespace().nth(1)).unwrap_or("/");
    let path = path.split('?').next().unwrap_or("/");
    match path {
        "/" | "/index.html" => respond(s, "200 OK", "text/html; charset=utf-8", SPLASH.as_bytes()),
        "/status" => respond(s, "200 OK", "application/json", status_json(o).as_bytes()),
        _ => respond(s, "404 Not Found", "text/plain", b"not found"),
    }
}

/// Exit once the app is up (the page has long since navigated away) or the supervisor has stopped.
fn watchdog(o: Arc<Opts>) {
    let started = Instant::now();
    let mut ready_since: Option<Instant> = None;
    loop {
        std::thread::sleep(Duration::from_secs(1));
        let s = read_file(&o.dir.join("services.json"));
        if s.contains("\"overall\":\"stopped\"") || started.elapsed() > Duration::from_secs(6 * 3600) {
            std::process::exit(0);
        }
        if s.contains("\"overall\":\"ready\"") {
            let t = ready_since.get_or_insert_with(Instant::now);
            if t.elapsed() > Duration::from_secs(30) {
                std::process::exit(0);
            }
        } else {
            ready_since = None;
        }
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

fn main() {
    let o = match parse_args() {
        Ok(o) => Arc::new(o),
        Err(e) => {
            eprintln!("launcher: {e}");
            std::process::exit(2);
        }
    };
    let listener = match TcpListener::bind(("127.0.0.1", o.port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("launcher: cannot listen on 127.0.0.1:{}: {e}", o.port);
            std::process::exit(1);
        }
    };
    let url = format!("http://127.0.0.1:{}/", o.port);
    println!("[launcher] loading screen: {url}");
    if o.open {
        open_window(&url);
    }
    {
        let o = o.clone();
        std::thread::spawn(move || watchdog(o));
    }
    for s in listener.incoming().flatten() {
        let o = o.clone();
        std::thread::spawn(move || handle(s, &o));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_crates_and_keeps_a_short_tail() {
        let log = "   Compiling libc v0.2.1\n   Compiling whisper-rs-sys v0.11.1 (/x)\n  Downloaded foo v1\nwarning: x\n";
        let (c, d, cur, tail) = build_stats(log);
        assert_eq!((c, d, cur.as_str()), (2, 1, "whisper-rs-sys"));
        assert_eq!(tail.len(), 4);
        let many: String = (0..20).map(|i| format!("line {i}\n")).collect();
        assert_eq!(build_stats(&many).3.len(), 8);
    }

    #[test]
    fn json_escaping_and_key_values() {
        assert_eq!(jstr("a\"b\\c\n"), "\"a\\\"b\\\\c\\n\"");
        let kv = key_values("phase=build\r\nlabel = Compiling the engine \r\nbroken line\n");
        assert_eq!(get(&kv, "phase"), "build");
        assert_eq!(get(&kv, "label"), "Compiling the engine");
        assert_eq!(get(&kv, "nope"), "");
    }

    #[test]
    fn status_without_any_files_is_valid_and_empty() {
        let o = Opts { port: 0, dir: PathBuf::from("/definitely/not/here"), root: PathBuf::from("/nope"), open: false };
        let j = status_json(&o);
        assert!(j.starts_with("{\"name\":\"\",\"phase\":\"\""), "{j}");
        assert!(j.ends_with("\"services\":null}"), "{j}");
    }
}
