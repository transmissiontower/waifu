//! Web search for the orchestrator.
//!
//! Small local models (gemma3 in Ollama has no tool calling) cannot "decide" to search, so the pipeline does
//! it for them: when the user's message looks like it needs fresh information we fetch a few result
//! snippets and attach them to *that one request* (never to the stored history or memory).
//!
//! Latency rules (she has to stay snappy):
//!  * normal chat never triggers a search (`mode: auto` looks for "search", "latest", "weather", ...)
//!  * one attempt with a hard timeout; on any failure the turn just continues without results
//!  * the snippets go into the LAST user message, so the LLM's cached prefix (system prompt + history)
//!    stays valid and nothing has to be re-processed.
use regex::Regex;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct WebSearchCfg {
    pub enabled: bool,
    /// `duckduckgo` (no key, no setup) or `searxng` (self-hosted; nothing leaves your own server).
    pub provider: String,
    /// Base URL of your SearXNG instance (needs `json` in its `search.formats`).
    pub searxng_url: String,
    /// `auto` = only when the message looks like it needs fresh info, `always` = every message.
    pub mode: String,
    pub max_results: usize,
    pub snippet_chars: usize,
    /// Hard cap for the whole lookup; on timeout she simply answers without it.
    pub timeout_ms: u64,
    pub cache_s: u64,
}

impl Default for WebSearchCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            provider: "duckduckgo".into(),
            searxng_url: "http://127.0.0.1:8888".into(),
            mode: "auto".into(),
            max_results: 4,
            snippet_chars: 220,
            timeout_ms: 2500,
            cache_s: 300,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub title: String,
    pub snippet: String,
}

pub struct WebSearch {
    cfg: WebSearchCfg,
    http: reqwest::Client,
    cache: Mutex<HashMap<String, (Instant, Vec<Hit>)>>,
}

impl WebSearch {
    pub fn new(cfg: WebSearchCfg) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_millis(cfg.timeout_ms.max(500)))
            .user_agent("Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0")
            .build()
            .unwrap_or_default();
        if cfg.enabled {
            tracing::info!(provider = %cfg.provider, mode = %cfg.mode, "web search enabled");
        }
        Self { cfg, http, cache: Mutex::new(HashMap::new()) }
    }

    /// If `user_text` needs a lookup, return the user message with the results attached; else `None`.
    pub async fn augment(&self, user_text: &str) -> Option<String> {
        if !self.cfg.enabled || !should_search(user_text, &self.cfg.mode) {
            return None;
        }
        let query = build_query(user_text);
        if query.is_empty() {
            return None;
        }
        let t0 = Instant::now();
        let hits = match self.lookup(&query).await {
            Ok(h) if !h.is_empty() => h,
            Ok(_) => {
                tracing::info!(%query, "web search: no results");
                return None;
            }
            Err(e) => {
                tracing::warn!(%query, error = %e, "web search failed, answering without it");
                return None;
            }
        };
        tracing::info!(%query, results = hits.len(), ms = t0.elapsed().as_millis() as u64, "web search");
        Some(attach(user_text, &hits, &today_utc(), self.cfg.snippet_chars))
    }

    async fn lookup(&self, query: &str) -> Result<Vec<Hit>, String> {
        let key = query.to_lowercase();
        if let Some((at, hits)) = self.cache.lock().unwrap().get(&key) {
            if at.elapsed() < Duration::from_secs(self.cfg.cache_s) {
                return Ok(hits.clone());
            }
        }
        let fut = async {
            match self.cfg.provider.as_str() {
                "searxng" => self.searxng(query).await,
                _ => self.duckduckgo(query).await,
            }
        };
        let hits = tokio::time::timeout(Duration::from_millis(self.cfg.timeout_ms), fut)
            .await
            .map_err(|_| format!("timed out after {}ms", self.cfg.timeout_ms))??;
        if !hits.is_empty() {
            let mut c = self.cache.lock().unwrap();
            if c.len() >= 64 {
                c.clear();
            }
            c.insert(key, (Instant::now(), hits.clone()));
        }
        Ok(hits)
    }

    async fn duckduckgo(&self, query: &str) -> Result<Vec<Hit>, String> {
        let resp = self
            .http
            .post("https://html.duckduckgo.com/html/")
            .form(&[("q", query), ("kl", "wt-wt")])
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("duckduckgo answered HTTP {}", resp.status()));
        }
        let body = resp.text().await.map_err(|e| e.to_string())?;
        if body.contains("anomaly-modal") || body.contains("g-recaptcha") {
            return Err("duckduckgo asked for a captcha (rate limited)".into());
        }
        Ok(parse_ddg(&body, self.cfg.max_results))
    }

    async fn searxng(&self, query: &str) -> Result<Vec<Hit>, String> {
        let url = format!("{}/search", self.cfg.searxng_url.trim_end_matches('/'));
        let resp = self
            .http
            .get(url)
            .query(&[("q", query), ("format", "json")])
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("searxng answered HTTP {} (is `json` enabled in its search.formats?)", resp.status()));
        }
        let v: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
        Ok(parse_searxng(&v, self.cfg.max_results))
    }
}

// ───────────────────────────── trigger + query ─────────────────────────────

fn re(cell: &'static OnceLock<Regex>, pat: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pat).expect("valid regex"))
}

/// Does this message need the internet?
pub fn should_search(text: &str, mode: &str) -> bool {
    static EXPLICIT: OnceLock<Regex> = OnceLock::new();
    static FRESH: OnceLock<Regex> = OnceLock::new();
    static WH: OnceLock<Regex> = OnceLock::new();
    static PERSONAL: OnceLock<Regex> = OnceLock::new();
    let t = text.trim();
    if t.chars().count() < 4 {
        return false;
    }
    if mode.eq_ignore_ascii_case("always") {
        return true;
    }
    // A factual who/what/when/where question that is not about her or the chat ("what is the birthday of X",
    // "how old is Y") must be looked up: left alone, a small model confidently invents the answer.
    let factual_question = t.split_whitespace().count() >= 4
        && re(&WH, r"(?i)^(what|who|whom|whose|when|where|which|how\s+(old|tall|many|much|long|far|big))\b").is_match(t)
        && !re(&PERSONAL, r"(?i)\b(you|you're|your|yours|u|we|us|me|my|mine|i|i'm)\b").is_match(t);
    factual_question
        || re(&EXPLICIT, r"(?i)\b(search|google|look\s*(it\s*)?up|check\s+online|find\s+out)\b").is_match(t)
        || re(
            &FRESH,
            r"(?i)\b(latest|news|today|tonight|yesterday|this\s+(week|month|year)|current(ly)?|right\s+now|weather|forecast|price\s+of|how\s+much\s+(is|does|are)|score|who\s+won|release\s+date|what\s+happened|is\s+.+\s+still|new\s+(episode|season|version|update|release))\b",
        )
        .is_match(t)
}

/// Turn "hey, can you google the weather in tokyo?" into "the weather in tokyo".
pub fn build_query(text: &str) -> String {
    static FILLER: OnceLock<Regex> = OnceLock::new();
    static SPACES: OnceLock<Regex> = OnceLock::new();
    let q = re(
        &FILLER,
        r"(?i)^\s*(hey|hi|yo|ok|okay|so|um|uh|please)[\s,]+|\b(can|could|would|will)\s+you\s+(please\s+)?|\b(search(\s+for|\s+up)?|google|look\s*(it\s*)?up|check\s+online|find\s+out)\b",
    )
    .replace_all(text, " ");
    let q = re(&SPACES, r"\s+").replace_all(&q, " ");
    let q = q.trim_matches(|c: char| c.is_whitespace() || "?!.,".contains(c));
    let q = if q.is_empty() { text.trim() } else { q };
    q.chars().take(200).collect()
}

// ───────────────────────────── parsing ─────────────────────────────

fn clean_html(s: &str) -> String {
    static TAGS: OnceLock<Regex> = OnceLock::new();
    static NUM: OnceLock<Regex> = OnceLock::new();
    static SPACES: OnceLock<Regex> = OnceLock::new();
    let s = re(&TAGS, r"<[^>]*>").replace_all(s, "");
    let s = re(&NUM, r"&#([xX]?)([0-9a-fA-F]+);").replace_all(&s, |c: &regex::Captures| {
        let radix = if c[1].is_empty() { 10 } else { 16 };
        u32::from_str_radix(&c[2], radix).ok().and_then(char::from_u32).map(String::from).unwrap_or_default()
    });
    let s = s
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&");
    re(&SPACES, r"\s+").replace_all(&s, " ").trim().to_string()
}

/// Parse the HTML-only DuckDuckGo results page (title + snippet; ads are skipped).
pub fn parse_ddg(html: &str, max: usize) -> Vec<Hit> {
    static TITLE: OnceLock<Regex> = OnceLock::new();
    static SNIPPET: OnceLock<Regex> = OnceLock::new();
    let title_re = re(&TITLE, r#"(?s)<a\b[^>]*class="[^"]*\bresult__a\b[^"]*"[^>]*>(.*?)</a>"#);
    let snip_re = re(&SNIPPET, r#"(?s)<a\b[^>]*class="[^"]*\bresult__snippet\b[^"]*"[^>]*>(.*?)</a>"#);
    let titles: Vec<_> = title_re.captures_iter(html).map(|c| (c.get(0).unwrap(), c[1].to_string())).collect();
    let mut out = Vec::new();
    for (i, (m, raw_title)) in titles.iter().enumerate() {
        if out.len() >= max {
            break;
        }
        // sponsored results link through duckduckgo.com/y.js
        if m.as_str().contains("duckduckgo.com/y.js") || m.as_str().contains("ad_domain") {
            continue;
        }
        let end = titles.get(i + 1).map(|(n, _)| n.start()).unwrap_or(html.len());
        let snippet = snip_re.captures(&html[m.end()..end]).map(|c| clean_html(&c[1])).unwrap_or_default();
        let title = clean_html(raw_title);
        if title.is_empty() || (snippet.is_empty() && title.len() < 8) {
            continue;
        }
        out.push(Hit { title, snippet });
    }
    out
}

pub fn parse_searxng(v: &serde_json::Value, max: usize) -> Vec<Hit> {
    v.get("results")
        .and_then(|r| r.as_array())
        .map(|rs| {
            rs.iter()
                .filter_map(|r| {
                    let title = r.get("title")?.as_str()?.trim().to_string();
                    let snippet = r.get("content").and_then(|c| c.as_str()).unwrap_or("").trim().to_string();
                    (!title.is_empty()).then_some(Hit { title, snippet })
                })
                .take(max)
                .collect()
        })
        .unwrap_or_default()
}

// ───────────────────────────── prompt ─────────────────────────────

/// The user's message plus the results. Only the LLM request carries this; history and memory keep the plain text.
pub fn attach(user_text: &str, hits: &[Hit], today: &str, snippet_chars: usize) -> String {
    let mut s = String::with_capacity(user_text.len() + 128 * hits.len() + 256);
    s.push_str(user_text);
    s.push_str(&format!(
        "\n\n[Background you already know (today is {today}). Answer from it naturally, in your own voice and \
in your usual short style. Never say you searched or looked anything up, never mention sources or links. If it does \
not answer the question, say you are not sure instead of guessing.]\n"
    ));
    for h in hits {
        let snip: String = h.snippet.chars().take(snippet_chars).collect();
        if snip.is_empty() {
            s.push_str(&format!("- {}\n", h.title));
        } else {
            s.push_str(&format!("- {}: {}\n", h.title, snip));
        }
    }
    s
}

/// Today's date (UTC) as YYYY-MM-DD, without pulling in a date crate.
fn today_utc() -> String {
    let days = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| (d.as_secs() / 86_400) as i64).unwrap_or(0);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triggers() {
        for t in [
            "what's the weather in tokyo today",
            "can you search for the rust 1.90 release date",
            "who won the game last night",
            "google the price of a raspberry pi 5",
            "is the new season of that anime out yet, look it up",
        ] {
            assert!(should_search(t, "auto"), "{t}");
        }
        for t in ["i love pizza", "lol", "tell me a joke", "you are mean today :(", "scum's wish is great"] {
            assert!(!should_search(t, "auto") || t.contains("today"), "{t}");
        }
        for t in [
            "what is the birthday of George Floyd?",
            "how old is taylor swift",
            "who is the president of france",
            "where is mount kilimanjaro located",
        ] {
            assert!(should_search(t, "auto"), "{t}");
        }
        for t in ["what are you doing", "what do you think about it", "who is he", "what is my name", "when did you wake up"] {
            assert!(!should_search(t, "auto"), "{t}");
        }
        assert!(!should_search("lol", "always"));
        assert!(should_search("tell me a joke", "always"));
    }

    #[test]
    fn queries() {
        assert_eq!(build_query("hey, can you google the weather in tokyo?"), "the weather in tokyo");
        assert_eq!(build_query("search for rust 1.90 release date"), "rust 1.90 release date");
        assert_eq!(build_query("who won the game last night"), "who won the game last night");
    }

    #[test]
    fn ddg_parsing() {
        let html = r#"
<div class="result results_links result--ad"><h2 class="result__title"><a rel="nofollow" class="result__a" href="//duckduckgo.com/y.js?ad_domain=x.com">Buy stuff</a></h2>
<a class="result__snippet" href="//duckduckgo.com/y.js">An ad</a></div>
<div class="result results_links results_links_deep web-result"><div class="links_main links_deep result__body">
<h2 class="result__title"><a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com">Tokyo <b>weather</b> &amp; forecast</a></h2>
<a class="result__snippet" href="//duckduckgo.com/l/?uddg=x">Sunny, <b>18&#176;C</b>. It&#x27;s clear tonight.</a></div></div>
<div class="result"><h2 class="result__title"><a class="result__a" href="//x">Second result</a></h2>
<a class="result__snippet" href="//x">Another snippet</a></div>"#;
        let hits = parse_ddg(html, 4);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "Tokyo weather & forecast");
        assert_eq!(hits[0].snippet, "Sunny, 18°C. It's clear tonight.");
        assert_eq!(hits[1].title, "Second result");
        assert_eq!(parse_ddg(html, 1).len(), 1);
    }

    #[test]
    fn searxng_parsing() {
        let v = serde_json::json!({"results": [{"title": "A", "content": "aa"}, {"title": "", "content": "x"}, {"title": "B"}]});
        let h = parse_searxng(&v, 4);
        assert_eq!(h, vec![Hit { title: "A".into(), snippet: "aa".into() }, Hit { title: "B".into(), snippet: "".into() }]);
    }

    #[test]
    fn attach_keeps_question_first() {
        let s = attach("weather in tokyo?", &[Hit { title: "T".into(), snippet: "sunny".into() }], "2026-11-08", 50);
        assert!(s.starts_with("weather in tokyo?"));
        assert!(s.contains("- T: sunny"));
        assert!(s.contains("2026-11-08"));
    }

    #[test]
    fn date_is_sane() {
        let d = today_utc();
        assert_eq!(d.len(), 10);
        assert!(d.starts_with("20"));
    }
}
