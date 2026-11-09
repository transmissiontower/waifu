use std::collections::BTreeMap;
use std::sync::Mutex;

/// Tiny Prometheus-text metrics registry (counters, gauges, duration sum/count).
#[derive(Default)]
pub struct Metrics {
    values: Mutex<BTreeMap<String, f64>>,
}

fn key(name: &str, labels: &[(&str, &str)]) -> String {
    if labels.is_empty() {
        name.to_string()
    } else {
        let l: Vec<String> = labels.iter().map(|(k, v)| format!("{k}=\"{v}\"")).collect();
        format!("{name}{{{}}}", l.join(","))
    }
}

impl Metrics {
    pub fn add(&self, name: &str, labels: &[(&str, &str)], v: f64) {
        *self.values.lock().unwrap().entry(key(name, labels)).or_insert(0.0) += v;
    }
    pub fn inc(&self, name: &str) {
        self.add(name, &[], 1.0);
    }
    pub fn inc_l(&self, name: &str, labels: &[(&str, &str)]) {
        self.add(name, labels, 1.0);
    }
    pub fn set(&self, name: &str, v: f64) {
        self.values.lock().unwrap().insert(key(name, &[]), v);
    }
    /// Records a duration in seconds as `<name>_seconds_sum` / `<name>_seconds_count`.
    pub fn observe(&self, name: &str, secs: f64) {
        self.add(&format!("{name}_seconds_sum"), &[], secs);
        self.add(&format!("{name}_seconds_count"), &[], 1.0);
    }
    pub fn get(&self, name: &str) -> f64 {
        self.values.lock().unwrap().get(name).copied().unwrap_or(0.0)
    }
    pub fn render(&self, service: &str) -> String {
        let mut out = String::new();
        for (k, v) in self.values.lock().unwrap().iter() {
            let line = match k.find('{') {
                Some(i) => format!("waifu_{}{{service=\"{service}\",{}", &k[..i], &k[i + 1..]),
                None => format!("waifu_{k}{{service=\"{service}\"}}"),
            };
            out.push_str(&format!("{line} {v}\n"));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn renders_with_service_label() {
        let m = Metrics::default();
        m.inc("requests_total");
        m.inc_l("rpc_total", &[("method", "Chat")]);
        m.observe("rpc", 0.5);
        let t = m.render("llm");
        assert!(t.contains("waifu_requests_total{service=\"llm\"} 1"));
        assert!(t.contains("waifu_rpc_total{service=\"llm\",method=\"Chat\"} 1"));
        assert!(t.contains("waifu_rpc_seconds_sum{service=\"llm\"} 0.5"));
    }
}
