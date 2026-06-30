//! Model performance tracking + adaptive routing. Bob learns which models are
//! fast, reliable, and produce quality code — then routes accordingly.
//!
//! Stats persist to `.bob/model-stats.json`. Each build records latency and
//! success. The fallback chain auto-reorders based on score. Dead endpoints
//! are skipped via a 3-second health check before each attempt.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

/// Local vLLM endpoint base (`.../v1`). Overridable via `BOB_VLLM_URL` so a
/// moved host doesn't require a recompile. The override is normalized so common
/// typos still work (missing scheme, trailing slash, missing `/v1`).
/// ponytail: single env knob, current default preserved.
pub fn vllm_url() -> String {
    match std::env::var("BOB_VLLM_URL") {
        Ok(u) => normalize_vllm_url(&u),
        Err(_) => "http://192.168.1.193:8000/v1".into(),
    }
}

/// Tidy a user-supplied vLLM base URL: prepend `http://` if no scheme, drop a
/// trailing slash, and append `/v1` (the OpenAI-compatible suffix) if absent.
fn normalize_vllm_url(raw: &str) -> String {
    let mut u = raw.trim().to_string();
    if u.is_empty() {
        return "http://192.168.1.193:8000/v1".into();
    }
    if !u.starts_with("http://") && !u.starts_with("https://") {
        u = format!("http://{u}");
    }
    let u = u.trim_end_matches('/');
    if u.ends_with("/v1") {
        u.to_string()
    } else {
        format!("{u}/v1")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelStats {
    pub runs: u32,
    pub successes: u32,
    pub avg_latency_secs: f64,
    pub last_latency_secs: f64,
    pub last_success: bool,
}

impl Default for ModelStats {
    fn default() -> Self {
        Self {
            runs: 0,
            successes: 0,
            avg_latency_secs: 45.0, // sane default for local models
            last_latency_secs: 0.0,
            last_success: false,
        }
    }
}

impl ModelStats {
    pub fn success_rate(&self) -> f64 {
        if self.runs == 0 {
            0.5 // unknown — neutral
        } else {
            self.successes as f64 / self.runs as f64
        }
    }

    /// Higher = better. Balances reliability vs speed.
    pub fn score(&self) -> f64 {
        let reliability = self.success_rate();
        let speed = 1.0 / self.avg_latency_secs.max(1.0);
        reliability * speed * 100.0
    }

    /// Adaptive timeout: 2× historical avg, clamped to [30s, 180s].
    /// A model that usually takes 40s gets killed at 80s, not 180s.
    pub fn adaptive_timeout(&self) -> Duration {
        let timeout = (self.avg_latency_secs * 2.0).max(30.0).min(180.0);
        Duration::from_secs(timeout as u64)
    }

    /// Should we pivot to another model? True if elapsed exceeds 1.5× the
    /// adaptive timeout (model is stuck well beyond its normal range).
    pub fn should_pivot(&self, elapsed: Duration) -> bool {
        elapsed.as_secs_f64() > self.avg_latency_secs * 1.5
    }

    pub fn record(&mut self, latency_secs: f64, success: bool) {
        self.runs += 1;
        if success {
            self.successes += 1;
        }
        // Rolling average
        self.avg_latency_secs =
            (self.avg_latency_secs * (self.runs - 1) as f64 + latency_secs) / self.runs as f64;
        self.last_latency_secs = latency_secs;
        self.last_success = success;
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct StatsStore {
    pub models: BTreeMap<String, ModelStats>,
}

impl StatsStore {
    pub fn load() -> Self {
        let path = Self::path();
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) {
        let path = Self::path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, json);
        }
    }

    fn path() -> PathBuf {
        PathBuf::from(".bob/model-stats.json")
    }

    /// Delete the stats file, resetting all learned rankings to cold start.
    /// Returns the removed path, or None if there was nothing to reset.
    pub fn reset() -> Option<PathBuf> {
        let path = Self::path();
        if path.exists() && std::fs::remove_file(&path).is_ok() {
            Some(path)
        } else {
            None
        }
    }

    pub fn get(&self, model: &str) -> &ModelStats {
        self.models.get(model).unwrap_or(&FALLBACK_STATS)
    }

    pub fn record(&mut self, model: &str, latency_secs: f64, success: bool) {
        let stats = self.models.entry(model.to_string()).or_default();
        stats.record(latency_secs, success);
    }

    /// Atomically record one run: lock, reload, update, persist. An exclusive
    /// flock on a sidecar lockfile serializes concurrent bob processes so they
    /// don't clobber each other's stats (lost updates from load→record→save
    /// interleaving). The critical section is a sub-millisecond file rewrite.
    /// ponytail: coarse whole-file lock; fine until stats writes get hot.
    pub fn record_run(model: &str, latency_secs: f64, success: bool) {
        use std::os::unix::io::AsRawFd;
        let path = Self::path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .open(path.with_extension("lock"))
            .ok();
        if let Some(f) = &lock {
            unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) };
        }
        let mut store = Self::load();
        store.record(model, latency_secs, success);
        store.save();
        // Lock releases when `lock` (the open File) drops here.
    }

    /// Rank models by score (best first). Models with no history get neutral score.
    pub fn rank(&self, models: &[String]) -> Vec<String> {
        let mut scored: Vec<(String, f64)> = models
            .iter()
            .map(|m| {
                let stats = self.get(m);
                (m.clone(), stats.score())
            })
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.into_iter().map(|(m, _)| m).collect()
    }

    /// 3-second health check: is the endpoint alive?
    /// Extracts base_url from the model id (provider/model → provider endpoint).
    pub fn health_check(model_id: &str) -> bool {
        // For opencode provider models (provider/model), we can't easily get
        // the endpoint URL without parsing opencode's config. Instead, we do
        // a lightweight check: can opencode reach this model?
        // For now, check if it's a known-local endpoint.
        if model_id.starts_with("ollama/") {
            return Self::curl_health(&format!("{}/models", vllm_url()));
        }
        if model_id.starts_with("192.168.1.") {
            let ip = model_id.split('/').next().unwrap_or("");
            return Self::curl_health(&format!("http://{ip}:8000/v1/models"));
        }
        // Cloud models (zai-coding-plan, minimax-coding-plan) — assume alive
        true
    }

    fn curl_health(url: &str) -> bool {
        let result = std::process::Command::new("curl")
            .args(["-s", "--max-time", "3", "-o", "/dev/null", "-w", "%{http_code}", url])
            .output();
        match result {
            Ok(out) => {
                let code = String::from_utf8_lossy(&out.stdout);
                code.trim().starts_with('2')
            }
            Err(_) => false,
        }
    }

    /// Print a summary table of model performance.
    pub fn print_summary(&self) {
        if self.models.is_empty() {
            println!("(no model stats yet)");
            return;
        }
        println!(
            "{:<45} {:>5} {:>6} {:>8} {:>8} {:>6}",
            "model", "runs", "succ%", "avg_s", "last_s", "score"
        );
        let mut entries: Vec<_> = self.models.iter().collect();
        entries.sort_by(|a, b| b.1.score().partial_cmp(&a.1.score()).unwrap_or(std::cmp::Ordering::Equal));
        for (model, stats) in entries {
            println!(
                "{:<45} {:>5} {:>5.0}% {:>7.1}s {:>7.1}s {:>6.1}",
                &model[..model.len().min(45)],
                stats.runs,
                stats.success_rate() * 100.0,
                stats.avg_latency_secs,
                stats.last_latency_secs,
                stats.score()
            );
        }
    }
}

static FALLBACK_STATS: ModelStats = ModelStats {
    runs: 0,
    successes: 0,
    avg_latency_secs: 45.0,
    last_latency_secs: 0.0,
    last_success: false,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_updates_avg() {
        let mut s = ModelStats::default();
        s.record(40.0, true);
        assert_eq!(s.avg_latency_secs, 40.0);
        s.record(60.0, true);
        assert_eq!(s.avg_latency_secs, 50.0); // (40+60)/2
    }

    #[test]
    fn score_balances_speed_and_reliability() {
        let fast_reliable = ModelStats { runs: 10, successes: 10, avg_latency_secs: 20.0, ..Default::default() };
        let slow_reliable = ModelStats { runs: 10, successes: 10, avg_latency_secs: 120.0, ..Default::default() };
        let fast_unreliable = ModelStats { runs: 10, successes: 3, avg_latency_secs: 20.0, ..Default::default() };
        assert!(fast_reliable.score() > slow_reliable.score());
        assert!(fast_reliable.score() > fast_unreliable.score());
    }

    #[test]
    fn adaptive_timeout_uses_history() {
        let fast = ModelStats { runs: 5, successes: 5, avg_latency_secs: 20.0, ..Default::default() };
        let slow = ModelStats { runs: 5, successes: 5, avg_latency_secs: 90.0, ..Default::default() };
        assert_eq!(fast.adaptive_timeout(), Duration::from_secs(40)); // 20*2
        assert_eq!(slow.adaptive_timeout(), Duration::from_secs(180)); // 90*2 = 180, clamped
    }

    #[test]
    fn rank_orders_by_score() {
        let mut store = StatsStore::default();
        store.record("fast", 20.0, true);
        store.record("fast", 20.0, true);
        store.record("slow", 100.0, true);
        store.record("slow", 100.0, true);
        let ranked = store.rank(&["slow".into(), "fast".into()]);
        assert_eq!(ranked[0], "fast");
    }

    #[test]
    fn normalize_vllm_url_fixes_common_typos() {
        // missing scheme
        assert_eq!(normalize_vllm_url("192.168.1.50:8000/v1"), "http://192.168.1.50:8000/v1");
        // missing /v1 suffix
        assert_eq!(normalize_vllm_url("http://host:8000"), "http://host:8000/v1");
        // trailing slash
        assert_eq!(normalize_vllm_url("http://host:8000/v1/"), "http://host:8000/v1");
        // missing scheme AND /v1
        assert_eq!(normalize_vllm_url("host:8000"), "http://host:8000/v1");
        // https preserved, already correct
        assert_eq!(normalize_vllm_url("https://host/v1"), "https://host/v1");
        // empty falls back to default
        assert_eq!(normalize_vllm_url("   "), "http://192.168.1.193:8000/v1");
    }

    #[test]
    fn record_run_persists_under_lock() {
        let _g = crate::CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("bob-stats-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&tmp).unwrap();

        StatsStore::record_run("m1", 30.0, true);
        StatsStore::record_run("m1", 50.0, true);
        let loaded = StatsStore::load();

        std::env::set_current_dir(prev).unwrap();
        let _ = std::fs::remove_dir_all(&tmp);
        assert_eq!(loaded.get("m1").runs, 2);
        assert_eq!(loaded.get("m1").successes, 2);
    }

    #[test]
    fn should_pivot_when_beyond_normal_range() {
        let stats = ModelStats { runs: 5, successes: 5, avg_latency_secs: 30.0, ..Default::default() };
        assert!(!stats.should_pivot(Duration::from_secs(30)));
        assert!(stats.should_pivot(Duration::from_secs(50))); // > 30*1.5=45
    }
}
