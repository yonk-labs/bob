use std::path::Path;
use std::time::{Duration, Instant};
use tokio::process::Command;

#[derive(Debug, Clone, Default)]
pub struct BuilderOutcome {
    pub stdout_tail: String,
    pub stderr_tail: String,
    pub failure_kind: String,
}

pub trait Builder {
    async fn build(&self, prompt: &str, workdir: &Path) -> anyhow::Result<BuilderOutcome>;
}

/// Dispatch enum — lets engine.rs pick the builder type at runtime without
/// trait objects (async traits aren't object-safe).
pub enum BuilderKind {
    Opencode(Opencode),
    Thin(ThinBuilder),
    Goose(GooseBuilder),
}

impl Builder for BuilderKind {
    async fn build(&self, prompt: &str, workdir: &Path) -> anyhow::Result<BuilderOutcome> {
        match self {
            BuilderKind::Opencode(b) => b.build(prompt, workdir).await,
            BuilderKind::Thin(b) => b.build(prompt, workdir).await,
            BuilderKind::Goose(b) => b.build(prompt, workdir).await,
        }
    }
}

// ── Opencode builder (full agent loop, ~10K+ context floor) ─────────────────

pub struct Opencode {
    pub cmd: String,
    pub timeout: Duration,
    pub args: Vec<String>,
    pub run_id: Option<String>,
}

// (Opencode implementation is further down in this file — unchanged)

// ── Thin builder (curl-based, zero tool schemas, context = task size only) ──

/// Minimal builder that calls an OpenAI-compatible endpoint directly via curl.
/// No agent loop, no tool schemas, no system prompt overhead. The model gets
/// exactly the task content (spec + test + current file + error feedback) —
/// nothing more. Context size is determined by the task, not the harness.
///
/// The model outputs file contents using a simple delimiter format:
///   === src/foo.js ===
///   <file contents>
///   === src/bar.js ===
///   <file contents>
///
/// Or for single-file slices, just the raw file contents.
pub struct ThinBuilder {
    pub model_id: String,
    pub base_url: String,
    pub api_key: Option<String>,
    pub timeout: Duration,
}

const THIN_SYSTEM: &str = "\
You are a code editor. The user gives you a task, spec, test, and current file contents.\n\
Your job: output the complete file contents for each file that needs to be created or modified.\n\
\n\
FORMAT — output each file like this:\n\
=== path/to/file.js ===\n\
<complete file contents>\n\
=== path/to/other.js ===\n\
<complete file contents>\n\
\n\
Rules:\n\
- Output ONLY file contents in the format above. No markdown fences. No explanations.\n\
- Include the COMPLETE file, not just the changed parts.\n\
- If the task says to fix an error, make the minimal change needed.\n\
- Match the API signature the test expects exactly.\n\
- Use CommonJS (module.exports) unless the existing code uses ESM (export).";

impl Builder for ThinBuilder {
    async fn build(&self, prompt: &str, workdir: &Path) -> anyhow::Result<BuilderOutcome> {
        // The prompt from bob lists context files by NAME (e.g., "- tests/foo.test.js").
        // opencode can read those files itself; the thin builder can't — the model
        // only sees the prompt text. So we read each file and embed its contents
        // inline before sending to the model.
        let enriched_prompt = enrich_with_file_contents(prompt, workdir);

        let body = serde_json::json!({
            "model": &self.model_id,
            "messages": [
                {"role": "system", "content": THIN_SYSTEM},
                {"role": "user", "content": &enriched_prompt},
            ],
            "temperature": 0.2,
            "max_tokens": 4096,
        });
        let body_str = serde_json::to_string(&body)?;

        let url = format!(
            "{}/chat/completions",
            self.base_url.trim_end_matches('/')
        );

        let mut cmd = Command::new("curl");
        cmd.arg("-s")
            .arg("--max-time")
            .arg(self.timeout.as_secs().to_string())
            .arg("-X")
            .arg("POST")
            .arg(&url)
            .arg("-H")
            .arg("Content-Type: application/json");

        if let Some(key) = &self.api_key {
            cmd.arg("-H").arg(format!("Authorization: Bearer {key}"));
        }

        cmd.arg("-d")
            .arg("@-")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .current_dir(workdir);

        let mut child = cmd.spawn()?;
        use tokio::io::AsyncWriteExt;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(body_str.as_bytes()).await?;
        }
        let output = child.wait_with_output().await?;

        if !output.status.success() {
            anyhow::bail!(
                "thin builder: curl failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let resp: serde_json::Value = serde_json::from_str(&stdout)
            .map_err(|e| anyhow::anyhow!("thin builder: parse response: {e}; {stdout}"))?;

        if let Some(err) = resp.get("error") {
            anyhow::bail!("thin builder: model API error: {err}");
        }

        let content = resp["choices"][0]["message"]["content"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("thin builder: no content in response"))?
            .to_string();

        // Parse file blocks from the model's response and write them
        let files_written = parse_and_write_files(&content, workdir)?;

        Ok(BuilderOutcome {
            stdout_tail: format!(
                "thin builder: wrote {} file(s)\n{}",
                files_written.len(),
                content.chars().take(2000).collect::<String>()
            ),
            stderr_tail: String::new(),
            failure_kind: "ok".into(),
        })
    }
}

/// Read files mentioned in the "## CONTEXT FILES" section of the prompt and
/// embed their contents inline. Without this, the thin builder's model only
/// sees file NAMES, not contents — it can't implement to a test it can't read.
fn enrich_with_file_contents(prompt: &str, workdir: &Path) -> String {
    let mut enriched = prompt.to_string();

    // Find file paths in the "## CONTEXT FILES" section
    let mut in_context = false;
    let mut file_paths: Vec<String> = Vec::new();

    for line in prompt.lines() {
        if line.starts_with("## CONTEXT FILES") || line.starts_with("## EDITABLE PATHS") {
            in_context = true;
            continue;
        }
        if line.starts_with("## ") {
            in_context = false;
            continue;
        }
        if in_context {
            if let Some(path) = line.trim().strip_prefix("- ") {
                file_paths.push(path.trim().to_string());
            }
        }
    }

    if file_paths.is_empty() {
        return enriched;
    }

    enriched.push_str("\n\n## FILE CONTENTS (read-only reference)\n");
    for path in &file_paths {
        let full = workdir.join(path);
        match std::fs::read_to_string(&full) {
            Ok(contents) => {
                let truncated = truncate_chars(&contents, 4000);
                enriched.push_str(&format!("\n--- {path} ---\n{truncated}\n"));
            }
            Err(_) => {
                enriched.push_str(&format!("\n--- {path} ---\n(file not found)\n"));
            }
        }
    }

    enriched
}

/// Parse the model's output into files and write them to the workdir.
/// Supports two formats:
/// 1. Delimited: "=== path ===\n<contents>\n=== path2 ===\n<contents>"
/// 2. Raw: entire output is a single file (caller must know the path)
fn parse_and_write_files(content: &str, workdir: &Path) -> anyhow::Result<Vec<String>> {
    let mut written = Vec::new();

    // Strip markdown fences if present
    let content = content.trim();
    let content = if content.starts_with("```") {
        let lines: Vec<&str> = content.lines().collect();
        if lines.len() >= 2 {
            lines[1..lines.len() - 1].join("\n")
        } else {
            content.to_string()
        }
    } else {
        content.to_string()
    };

    // Check for delimited format: === path ===
    if content.contains("=== ") {
        let mut current_path: Option<String> = None;
        let mut current_contents = String::new();

        for line in content.lines() {
            if let Some(path) = extract_path_delimiter(line) {
                // Write previous file if any
                if let Some(path) = current_path.take() {
                    let full = workdir.join(&path);
                    if let Some(parent) = full.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    std::fs::write(&full, current_contents.trim())?;
                    written.push(path);
                    current_contents.clear();
                }
                current_path = Some(path);
            } else if current_path.is_some() {
                current_contents.push_str(line);
                current_contents.push('\n');
            }
        }
        // Write last file
        if let Some(path) = current_path {
            let full = workdir.join(&path);
            if let Some(parent) = full.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            std::fs::write(&full, current_contents.trim())?;
            written.push(path);
        }
    } else {
        // Raw format — can't determine path, write to a default location.
        // This happens when the model ignores the delimiter format.
        // Write to the first editable path if known, otherwise fail.
        eprintln!(
            "thin builder: model didn't use delimiter format, writing raw output to src/generated.js"
        );
        std::fs::create_dir_all(workdir.join("src"))?;
        std::fs::write(workdir.join("src/generated.js"), content)?;
        written.push("src/generated.js".into());
    }

    Ok(written)
}

fn extract_path_delimiter(line: &str) -> Option<String> {
    let line = line.trim();
    if line.starts_with("=== ") && line.ends_with(" ===") {
        let path = &line[4..line.len() - 4];
        if !path.is_empty() {
            return Some(path.to_string());
        }
    }
    None
}

// ── Goose builder (stripped extensions, smaller context floor) ──────────────

/// Adapter for Goose CLI. Goose with stripped extensions lands at ~2-3K context
/// floor (vs opencode's ~10K). Uses Goose's agent loop for multi-step edits
/// but without the massive tool schema overhead.
///
/// Install: `curl -fsSL https://github.com/block/goose/releases/latest/download/install.sh | bash`
/// Configure: strip to single extension (developer) + tiny_model_system.md
/// Derive goose's `OPENAI_HOST` (host only) from a full base URL. goose appends
/// `OPENAI_BASE_PATH` (default `v1/chat/completions`), so the host must NOT carry
/// a trailing `/v1` or slash — otherwise the request path doubles to `/v1/v1/…`.
/// Trims a trailing slash first so a user-written `…/v1/` normalizes the same as
/// `…/v1`. Works for local vLLM (`http://host:8000/v1`) and OpenAI cloud
/// (`https://api.openai.com/v1` → `https://api.openai.com`) alike.
fn openai_host(url: &str) -> &str {
    url.trim_end_matches('/').trim_end_matches("/v1").trim_end_matches('/')
}

pub struct GooseBuilder {
    pub cmd: String,
    pub model: String,
    pub timeout: Duration,
    pub provider: String,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    /// Set GOOSE_TOOLSHIM=true — interpret tool calls from plain-text output when
    /// the endpoint can't return structured tool_calls (see builder.goose_toolshim).
    pub toolshim: bool,
    /// When set, write `.bob/runs/<run_id>/goose.pid` for the reaper — same
    /// contract as Opencode's `opencode.pid`.
    pub run_id: Option<String>,
    /// Idle-stall watchdog threshold (builder.idle_stall_secs). Zero disables.
    /// Kill early when the endpoint shows no running request for this long.
    pub idle_stall: Duration,
}

/// SIGTERM → 200ms grace → SIGKILL, addressed to the PROCESS GROUP (`-pid`).
/// Both builders are setsid'd, so pgid == pid and group signals reach
/// grandchildren — a killed goose must not leave a tool child alive and
/// writing (finding #31's orphan risk). The direct pid is signaled too as a
/// belt-and-suspenders for a child that somehow isn't a group leader.
fn kill_group_with_escalation(pid: u32) {
    let pgid = -(pid as i32);
    unsafe {
        let _ = libc::kill(pgid, libc::SIGTERM);
        let _ = libc::kill(pid as i32, libc::SIGTERM);
    }
    std::thread::sleep(Duration::from_millis(200));
    unsafe {
        let _ = libc::kill(pgid, libc::SIGKILL);
        let _ = libc::kill(pid as i32, libc::SIGKILL);
    }
}

/// Reaper-visible pidfile for a builder child: `.bob/runs/<run_id>/<name>`.
fn builder_pidfile(run_id: &str, name: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(".bob/runs").join(run_id).join(name)
}

/// What the idle-stall watchdog should do at one poll tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleAction {
    /// Endpoint is busy or unobservable — reset the idle timer, keep waiting.
    ResetTimer,
    /// Confirmed idle, but not long enough yet — keep waiting, keep the timer.
    Wait,
    /// Confirmed idle past the threshold — kill the attempt early.
    KillIdle,
}

/// Pure idle-stall decision (F8). Kill ONLY when the endpoint answered with
/// zero running requests (`Some(false)`) continuously for `idle_stall`. A busy
/// endpoint (`Some(true)`) or an unobservable one (`None`, e.g. no /metrics)
/// resets the timer and is NEVER killed — a busy-loop stays governed by the
/// no-progress diff check + wall clock, exactly as the constraint requires.
/// `idle_stall == 0` disables the watchdog.
fn idle_watchdog_decision(
    idle_stall: Duration,
    idle_elapsed: Duration,
    running: Option<bool>,
) -> IdleAction {
    if idle_stall.is_zero() {
        return IdleAction::ResetTimer;
    }
    match running {
        Some(true) | None => IdleAction::ResetTimer,
        Some(false) if idle_elapsed >= idle_stall => IdleAction::KillIdle,
        Some(false) => IdleAction::Wait,
    }
}

impl Builder for GooseBuilder {
    async fn build(&self, prompt: &str, workdir: &Path) -> anyhow::Result<BuilderOutcome> {
        let mut cmd = Command::new(&self.cmd);
        cmd.arg("run")
            .arg("--no-profile")
            .arg("--with-builtin")
            .arg("developer")
            .arg("--quiet")
            .arg("--text")
            .arg(prompt)
            .arg("--model")
            .arg(&self.model)
            .arg("--provider")
            .arg(&self.provider)
            .current_dir(workdir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        // Point goose at the local endpoint. goose's `openai` provider reads
        // OPENAI_HOST (host only, no /v1 — it appends OPENAI_BASE_PATH), NOT
        // OPENAI_BASE_URL. Setting only the latter silently targets api.openai.com,
        // every request fails auth, goose makes no tool calls, and bob reports an
        // empty diff with no error. Set both: HOST for goose, BASE_URL for others.
        if let Some(url) = &self.base_url {
            cmd.env("OPENAI_HOST", openai_host(url));
            cmd.env("OPENAI_BASE_URL", url);
            cmd.env("OPENAI_API_KEY", self.api_key.as_deref().unwrap_or("local"));
        }

        // Interpret tool calls from plain-text output when the server can't return
        // structured tool_calls. Opt-in via builder.goose_toolshim (env still wins
        // if the operator sets GOOSE_TOOLSHIM directly).
        if self.toolshim {
            cmd.env("GOOSE_TOOLSHIM", "true");
        }

        // Point goose's rolling log file at a writable temp dir. We run with
        // --no-profile and pass all config via flags/env, so goose reads nothing
        // from XDG_CONFIG_HOME/HOME — only its logs use the state dir. Without this,
        // goose panics ("failed to create log file") when ~/.local/state is read-only
        // (containers, sandboxed CI), and it keeps log noise out of the worktree.
        cmd.env("XDG_STATE_HOME", std::env::temp_dir().join("bob-goose-state"));

        // setsid for process group isolation
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }

        let child = cmd
            .spawn()
            .map_err(|e| anyhow::anyhow!("spawning goose '{}': {e}", self.cmd))?;
        let child_pid = child.id();

        // Pidfile for the reaper (same contract as opencode.pid): if bob dies
        // without cleaning up, reap_orphans can find and kill this goose.
        if let (Some(run_id), Some(pid)) = (&self.run_id, child_pid) {
            let pid_path = builder_pidfile(run_id, "goose.pid");
            if let Some(parent) = pid_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&pid_path, pid.to_string());
        }
        let remove_pidfile = || {
            if let Some(run_id) = &self.run_id {
                let _ = std::fs::remove_file(builder_pidfile(run_id, "goose.pid"));
            }
        };

        // Race the process against the wall-clock deadline AND an idle-stall
        // watchdog (F8). goose is one-shot (`run --text`, stdin null,
        // wait_with_output buffers) so bob can't observe incremental output or
        // poke it — the only bounded response to an idle-wait hang is to kill
        // early and let the fallback wrapper hop. The watchdog polls the
        // endpoint's running-request signal; it never acts while a request is
        // running (busy-loop) or when the signal is unobservable (fail-safe).
        let deadline = Instant::now() + self.timeout;
        let poll = self.idle_poll_period();
        let running_probe = || {
            self.base_url
                .as_deref()
                .and_then(|u| crate::doctor::endpoint_running_request(u, self.api_key.as_deref()))
        };
        let wait = child.wait_with_output();
        tokio::pin!(wait);
        let mut last_active = Instant::now();
        let outcome = loop {
            tokio::select! {
                out = &mut wait => break WaitOutcome::Done(out),
                _ = tokio::time::sleep(poll) => {
                    if Instant::now() >= deadline {
                        break WaitOutcome::WallTimeout;
                    }
                    match idle_watchdog_decision(self.idle_stall, last_active.elapsed(), running_probe()) {
                        IdleAction::ResetTimer => last_active = Instant::now(),
                        IdleAction::Wait => {}
                        IdleAction::KillIdle => break WaitOutcome::IdleStall(last_active.elapsed()),
                    }
                }
            }
        };

        match outcome {
            WaitOutcome::Done(out) => {
                remove_pidfile();
                let out = out?;
                let stdout_tail = tail(&String::from_utf8_lossy(&out.stdout), 4000);
                let stderr_tail = tail(&String::from_utf8_lossy(&out.stderr), 4000);
                if !out.status.success() {
                    anyhow::bail!(
                        "goose exited with status {}; stderr:\n{}",
                        out.status,
                        stderr_tail
                    );
                }
                // goose exits 0 after "Network error: Request timed out — …"
                // against a dead endpoint, with zero tool calls made (repro
                // F2b). Surface that as endpoint_error instead of "ok" so the
                // engine can classify marker + empty diff as an INFRA error
                // and hop models, not burn a judge iteration on nothing.
                let network_err = stdout_tail.contains("Network error:")
                    || stderr_tail.contains("Network error:");
                Ok(BuilderOutcome {
                    stdout_tail,
                    stderr_tail,
                    failure_kind: if network_err {
                        "endpoint_error".into()
                    } else {
                        "ok".into()
                    },
                })
            }
            // Escalated GROUP kill for both terminal cases: kill_on_drop alone
            // SIGKILLs only the direct goose pid, orphaning any tool child.
            WaitOutcome::WallTimeout => {
                if let Some(pid) = child_pid {
                    kill_group_with_escalation(pid);
                }
                remove_pidfile();
                anyhow::bail!("goose timed out after {:?}", self.timeout)
            }
            WaitOutcome::IdleStall(elapsed) => {
                if let Some(pid) = child_pid {
                    kill_group_with_escalation(pid);
                }
                remove_pidfile();
                // "idle-stall" is the classified marker the engine maps to a
                // builder_idle_stall event and the fallback wrapper hops on.
                anyhow::bail!(
                    "goose idle-stalled after {elapsed:?} with no running request on the endpoint — killed early"
                )
            }
        }
    }
}

impl GooseBuilder {
    /// Watchdog poll cadence: frequent enough to notice within a fraction of
    /// the threshold, but never sub-second. Derived from idle_stall (¼ of it,
    /// clamped [2s, 15s]); when disabled, poll rarely — only the wall-clock
    /// deadline matters.
    fn idle_poll_period(&self) -> Duration {
        if self.idle_stall.is_zero() {
            return Duration::from_secs(15);
        }
        let quarter = self.idle_stall / 4;
        quarter.clamp(Duration::from_secs(2), Duration::from_secs(15))
    }
}

/// Terminal outcome of the goose wait/watchdog race.
enum WaitOutcome {
    Done(std::io::Result<std::process::Output>),
    WallTimeout,
    IdleStall(Duration),
}

// ── Opencode builder implementation (unchanged, moved here for cohesion) ────

impl Builder for Opencode {
    async fn build(&self, prompt: &str, workdir: &Path) -> anyhow::Result<BuilderOutcome> {
        let mut cmd = Command::new(&self.cmd);
        cmd.arg("run")
            .arg("--pure")    // strip external plugins — reduces system prompt overhead
            .arg("--dir")
            .arg(workdir)
            .args(&self.args)
            .arg(prompt)
            .current_dir(workdir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let child = cmd
            .spawn()
            .map_err(|e| anyhow::anyhow!("spawning builder '{}': {e}", self.cmd))?;
        let child_pid = child.id();

        if let Some(run_id) = &self.run_id {
            let pid_path = std::path::PathBuf::from(".bob/runs")
                .join(run_id)
                .join("opencode.pid");
            if let Some(parent) = pid_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Some(pid) = child_pid {
                let _ = std::fs::write(&pid_path, pid.to_string());
            }
        }

        match tokio::time::timeout(self.timeout, child.wait_with_output()).await {
            Ok(out) => {
                let out = out?;
                let stdout_tail = tail(&String::from_utf8_lossy(&out.stdout), 4000);
                let stderr_tail = tail(&String::from_utf8_lossy(&out.stderr), 4000);
                if let Some(run_id) = &self.run_id {
                    let pid_path = std::path::PathBuf::from(".bob/runs")
                        .join(run_id)
                        .join("opencode.pid");
                    let _ = std::fs::remove_file(&pid_path);
                }
                if !out.status.success() {
                    anyhow::bail!(
                        "builder exited with status {}; stderr tail:\n{}",
                        out.status,
                        stderr_tail
                    );
                }
                Ok(BuilderOutcome {
                    stdout_tail,
                    stderr_tail,
                    failure_kind: "ok".into(),
                })
            }
            Err(_) => {
                // Group kill: opencode is setsid'd too — signaling only the
                // direct pid orphans its grandchildren.
                if let Some(pid) = child_pid {
                    kill_group_with_escalation(pid);
                }
                if let Some(run_id) = &self.run_id {
                    let pid_path = std::path::PathBuf::from(".bob/runs")
                        .join(run_id)
                        .join("opencode.pid");
                    let _ = std::fs::remove_file(&pid_path);
                }
                anyhow::bail!("builder timed out after {:?}", self.timeout)
            }
        }
    }
}

pub fn tail(s: &str, max_chars: usize) -> String {
    let mut chars = s.chars().rev().take(max_chars).collect::<Vec<_>>();
    chars.reverse();
    chars.into_iter().collect()
}

/// First `max_chars` characters, char-boundary safe, with a truncation marker
/// appended if the input was longer. Used to cap context-file embeds without
/// panicking on multibyte UTF-8 (a raw `&s[..n]` byte slice would).
pub fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() > max_chars {
        let head: String = s.chars().take(max_chars).collect();
        format!("{head}...\n(truncated)")
    } else {
        s.to_string()
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_host_strips_v1_for_local_and_cloud() {
        // local vLLM
        assert_eq!(openai_host("http://192.168.1.193:8000/v1"), "http://192.168.1.193:8000");
        // user-written trailing slash must not double the /v1
        assert_eq!(openai_host("http://host:8000/v1/"), "http://host:8000");
        // OpenAI cloud
        assert_eq!(openai_host("https://api.openai.com/v1"), "https://api.openai.com");
        // already host-only (no /v1) — unchanged
        assert_eq!(openai_host("http://host:8000"), "http://host:8000");
        assert_eq!(openai_host("https://api.openai.com"), "https://api.openai.com");
    }

    #[test]
    fn parse_delimited_files() {
        let dir = tempdir();
        let content = "\
=== src/foo.js ===
const x = 1;
=== src/bar.js ===
const y = 2;
";
        let written = parse_and_write_files(content, &dir).unwrap();
        assert_eq!(written.len(), 2);
        assert_eq!(written[0], "src/foo.js");
        assert_eq!(written[1], "src/bar.js");
        assert_eq!(
            std::fs::read_to_string(dir.join("src/foo.js")).unwrap(),
            "const x = 1;"
        );
    }

    #[test]
    fn parse_single_delimited_file() {
        let dir = tempdir();
        let content = "\
=== src/body.js ===
class Body { }
";
        let written = parse_and_write_files(content, &dir).unwrap();
        assert_eq!(written, vec!["src/body.js"]);
        assert!(std::fs::read_to_string(dir.join("src/body.js")).unwrap().contains("class Body"));
    }

    #[test]
    fn truncate_chars_is_utf8_safe() {
        // 4001 two-byte chars: a raw &s[..4000] byte slice would panic mid-char.
        let s = "é".repeat(4001);
        let out = truncate_chars(&s, 4000);
        assert!(out.ends_with("(truncated)"));
        assert!(out.starts_with('é'));
        // Short input is returned unchanged, no marker.
        assert_eq!(truncate_chars("hi", 4000), "hi");
    }

    #[test]
    fn extract_delimiter() {
        assert_eq!(
            extract_path_delimiter("=== src/foo.js ==="),
            Some("src/foo.js".into())
        );
        assert_eq!(extract_path_delimiter("=== not a delimiter"), None);
        assert_eq!(extract_path_delimiter("const x = 1;"), None);
    }

    /// Write an executable fake builder script that records its argv (one per
    /// line) to `args.txt` in its cwd, then exits 0.
    fn write_argv_recorder(dir: &Path) -> std::path::PathBuf {
        let script = dir.join("fake-builder.sh");
        std::fs::write(&script, "#!/bin/sh\nprintf '%s\\n' \"$@\" > args.txt\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    /// repro F1: each builder kind must exec with ITS OWN flag set — goose
    /// must never receive opencode's `--pure`/`--dir`, opencode must.
    #[tokio::test]
    async fn opencode_and_goose_compose_their_own_argv() {
        // opencode: run --pure --dir <workdir> ... prompt
        let dir = tempdir();
        let script = write_argv_recorder(&dir);
        let b = Opencode {
            cmd: script.to_string_lossy().into_owned(),
            timeout: Duration::from_secs(5),
            args: vec![],
            run_id: None,
        };
        b.build("the prompt", &dir).await.unwrap();
        let argv = std::fs::read_to_string(dir.join("args.txt")).unwrap();
        let args: Vec<&str> = argv.lines().collect();
        assert_eq!(args[0], "run");
        assert!(args.contains(&"--pure"), "opencode gets --pure: {args:?}");
        assert!(args.contains(&"--dir"), "opencode gets --dir: {args:?}");

        // goose: run --no-profile ... --provider <p>, and NEVER opencode flags
        let dir = tempdir();
        let script = write_argv_recorder(&dir);
        let b = GooseBuilder {
            cmd: script.to_string_lossy().into_owned(),
            model: "m".into(),
            timeout: Duration::from_secs(5),
            provider: "openai".into(),
            base_url: None,
            api_key: None,
            toolshim: false,
            idle_stall: Duration::from_secs(0),
            run_id: None,
        };
        b.build("the prompt", &dir).await.unwrap();
        let argv = std::fs::read_to_string(dir.join("args.txt")).unwrap();
        let args: Vec<&str> = argv.lines().collect();
        assert_eq!(args[0], "run");
        assert!(args.contains(&"--no-profile"), "goose gets --no-profile: {args:?}");
        assert!(args.contains(&"--provider"), "goose gets --provider: {args:?}");
        assert!(!args.contains(&"--pure"), "goose must NOT get opencode's --pure: {args:?}");
        assert!(!args.contains(&"--dir"), "goose must NOT get opencode's --dir: {args:?}");
    }

    /// repro F2b: goose exits 0 after "Network error: Request timed out" with
    /// zero tokens — that must surface as endpoint_error, never "ok".
    #[tokio::test]
    async fn goose_exit_zero_network_error_is_endpoint_error_not_ok() {
        let dir = tempdir();
        let script = dir.join("fake-goose.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\necho 'Network error: Request timed out — check your network connection and try again.'\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mk = |cmd: String| GooseBuilder {
            cmd,
            model: "m".into(),
            timeout: Duration::from_secs(5),
            provider: "openai".into(),
            base_url: None,
            api_key: None,
            toolshim: false,
            idle_stall: Duration::from_secs(0),
            run_id: None,
        };
        let out = mk(script.to_string_lossy().into_owned())
            .build("p", &dir)
            .await
            .unwrap();
        assert_eq!(out.failure_kind, "endpoint_error");

        // Healthy exit-0 output stays "ok".
        let script_ok = dir.join("fake-goose-ok.sh");
        std::fs::write(&script_ok, "#!/bin/sh\necho 'done editing'\n").unwrap();
        std::fs::set_permissions(&script_ok, std::fs::Permissions::from_mode(0o755)).unwrap();
        let out = mk(script_ok.to_string_lossy().into_owned())
            .build("p", &dir)
            .await
            .unwrap();
        assert_eq!(out.failure_kind, "ok");
    }

    /// F8: the pure idle-stall decision. The two constraints that matter —
    /// never act on a busy endpoint, never act on an unobservable one — are
    /// asserted directly.
    #[test]
    fn idle_watchdog_only_kills_confirmed_idle_past_threshold() {
        let stall = Duration::from_secs(120);
        let long = Duration::from_secs(200);
        let short = Duration::from_secs(30);

        // Confirmed idle (no running request) past the threshold → kill.
        assert_eq!(
            idle_watchdog_decision(stall, long, Some(false)),
            IdleAction::KillIdle
        );
        // Confirmed idle but not long enough yet → keep waiting.
        assert_eq!(
            idle_watchdog_decision(stall, short, Some(false)),
            IdleAction::Wait
        );
        // BUSY (a request IS running), even long past the threshold → never
        // act; reset the timer. This is the core safety constraint.
        assert_eq!(
            idle_watchdog_decision(stall, long, Some(true)),
            IdleAction::ResetTimer
        );
        // UNOBSERVABLE endpoint (no /metrics) → fail-safe: never kill.
        assert_eq!(
            idle_watchdog_decision(stall, long, None),
            IdleAction::ResetTimer
        );
        // Disabled (idle_stall == 0) → never accumulate or kill.
        assert_eq!(
            idle_watchdog_decision(Duration::ZERO, long, Some(false)),
            IdleAction::ResetTimer
        );
    }

    #[test]
    fn idle_poll_period_is_bounded() {
        let mk = |secs| GooseBuilder {
            cmd: "goose".into(),
            model: "m".into(),
            timeout: Duration::from_secs(600),
            provider: "openai".into(),
            base_url: None,
            api_key: None,
            toolshim: false,
            idle_stall: Duration::from_secs(secs),
            run_id: None,
        };
        // quarter of 120 = 30 → clamped to the 15s ceiling.
        assert_eq!(mk(120).idle_poll_period(), Duration::from_secs(15));
        // quarter of 40 = 10 → within bounds.
        assert_eq!(mk(40).idle_poll_period(), Duration::from_secs(10));
        // quarter of 4 = 1 → clamped to the 2s floor (no thrashing).
        assert_eq!(mk(4).idle_poll_period(), Duration::from_secs(2));
        // disabled → rare poll, only the wall clock matters.
        assert_eq!(mk(0).idle_poll_period(), Duration::from_secs(15));
    }

    /// F7: a goose timeout must kill the whole PROCESS GROUP, not just the
    /// direct child — a surviving grandchild is exactly the #31 orphan risk.
    #[tokio::test]
    async fn goose_timeout_kills_the_whole_process_group() {
        let dir = tempdir();
        let script = dir.join("fake-goose-hang.sh");
        // Backgrounds a long-lived grandchild (same setsid'd process group),
        // records its pid, then hangs until killed.
        std::fs::write(
            &script,
            "#!/bin/sh\nsleep 300 &\necho $! > grandchild.pid\nwait\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let b = GooseBuilder {
            cmd: script.to_string_lossy().into_owned(),
            model: "m".into(),
            timeout: Duration::from_millis(500),
            provider: "openai".into(),
            base_url: None,
            api_key: None,
            toolshim: false,
            idle_stall: Duration::from_secs(0),
            run_id: None,
        };
        let res = b.build("p", &dir).await;
        assert!(res.is_err(), "hung goose must time out");

        let gpid: i32 = std::fs::read_to_string(dir.join("grandchild.pid"))
            .expect("grandchild pid recorded before the hang")
            .trim()
            .parse()
            .unwrap();
        // The group SIGKILL must take the grandchild down (allow reaping lag;
        // a zombie counts as dead — it can't write anything).
        let mut dead = false;
        for _ in 0..30 {
            let stat = std::fs::read_to_string(format!("/proc/{gpid}/stat"));
            match stat {
                Err(_) => {
                    dead = true;
                    break;
                }
                Ok(s) if s.contains(") Z ") => {
                    dead = true;
                    break;
                }
                Ok(_) => std::thread::sleep(Duration::from_millis(100)),
            }
        }
        assert!(dead, "grandchild pid={gpid} survived the timeout kill");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// F8 fail-safe (through the real watchdog loop): a hung goose whose
    /// endpoint is UNOBSERVABLE (no base_url → running-probe returns None)
    /// must NOT be idle-killed — it rides to the wall-clock timeout. Proves
    /// the watchdog never acts on an endpoint it can't read.
    #[tokio::test]
    async fn idle_watchdog_never_kills_an_unobservable_endpoint() {
        let dir = tempdir();
        let script = dir.join("fake-goose-hang.sh");
        std::fs::write(&script, "#!/bin/sh\nsleep 300\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let b = GooseBuilder {
            cmd: script.to_string_lossy().into_owned(),
            model: "m".into(),
            timeout: Duration::from_secs(3), // wall-clock backstop
            provider: "openai".into(),
            base_url: None, // unobservable → running-probe is None → fail-safe
            api_key: None,
            toolshim: false,
            idle_stall: Duration::from_secs(1), // would fire fast IF it could observe
            run_id: None,
        };
        let err = b.build("p", &dir).await.unwrap_err().to_string();
        assert!(
            err.contains("timed out") && !err.contains("idle-stall"),
            "unobservable endpoint must hit the WALL timeout, not idle-stall: {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// F7: reap_orphans covers goose.pid with the same contract as opencode.pid.
    #[test]
    fn reaper_cleans_dead_goose_pidfile() {
        let _cwd_guard = crate::CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempdir();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&tmp).unwrap();

        let run_dir = tmp.join(".bob/runs/r-goose");
        std::fs::create_dir_all(&run_dir).unwrap();
        // A pid that cannot exist (> kernel pid_max) — reads as dead.
        std::fs::write(run_dir.join("goose.pid"), "999999999").unwrap();

        let report = reap_orphans().unwrap();
        std::env::set_current_dir(prev).unwrap();

        assert!(report.cleaned >= 1, "dead goose pidfile counted as cleaned");
        assert!(
            !run_dir.join("goose.pid").exists(),
            "dead goose pidfile removed"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// F7: goose writes its reaper pidfile under .bob/runs/<run_id>/ and
    /// removes it on a clean exit.
    #[tokio::test]
    async fn goose_pidfile_removed_after_clean_exit() {
        let _cwd_guard = crate::CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempdir();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&tmp).unwrap();

        let script = tmp.join("fake-goose-ok.sh");
        std::fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let b = GooseBuilder {
            cmd: script.to_string_lossy().into_owned(),
            model: "m".into(),
            timeout: Duration::from_secs(5),
            provider: "openai".into(),
            base_url: None,
            api_key: None,
            toolshim: false,
            idle_stall: Duration::from_secs(0),
            run_id: Some("gpid-clean".into()),
        };
        let res = b.build("p", &tmp).await;
        let pidfile = tmp.join(".bob/runs/gpid-clean/goose.pid");
        let pidfile_exists = pidfile.exists();
        std::env::set_current_dir(prev).unwrap();

        res.unwrap();
        assert!(!pidfile_exists, "goose.pid removed after clean exit");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn times_out_a_hung_builder() {
        let b = ShimSleep { timeout: Duration::from_millis(200) };
        let res = b.build("ignored", Path::new(".")).await;
        assert!(res.is_err(), "hung builder must time out");
    }

    struct ShimSleep { timeout: Duration }
    impl Builder for ShimSleep {
        async fn build(&self, _p: &str, _w: &Path) -> anyhow::Result<BuilderOutcome> {
            let mut child = Command::new("sleep").arg("30").kill_on_drop(true).spawn()?;
            match tokio::time::timeout(self.timeout, child.wait()).await {
                Ok(s) => { s?; Ok(BuilderOutcome::default()) }
                Err(_) => { let _ = child.start_kill(); anyhow::bail!("timed out") }
            }
        }
    }

    fn tempdir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bob-thin-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}

// ── Reaper (unchanged) ──────────────────────────────────────────────────────

pub fn reap_orphans() -> anyhow::Result<ReapReport> {
    let mut report = ReapReport::default();
    let runs_dir = std::path::PathBuf::from(".bob/runs");
    if runs_dir.exists() {
        for entry in std::fs::read_dir(&runs_dir)? {
            let entry = entry?;
            // Both builder kinds write a reaper pidfile (goose since F7).
            for name in ["opencode.pid", "goose.pid"] {
                let pid_file = entry.path().join(name);
                if !pid_file.exists() { continue; }
                let pid_str = std::fs::read_to_string(&pid_file)?;
                let Ok(pid) = pid_str.trim().parse::<u32>() else { continue };
                let alive = unsafe { libc::kill(pid as i32, 0) == 0 };
                if !alive {
                    let _ = std::fs::remove_file(&pid_file);
                    report.cleaned += 1;
                    continue;
                }
                let ppid = read_ppid(pid);
                if let Some(ppid) = ppid {
                    let parent_alive = unsafe { libc::kill(ppid as i32, 0) == 0 };
                    if !parent_alive {
                        // Builders are setsid'd — group kill reaches their
                        // grandchildren, not just the leader.
                        kill_group_with_escalation(pid);
                        let _ = std::fs::remove_file(&pid_file);
                        report.orphans_killed += 1;
                        eprintln!("reaper: killed orphan builder pid={pid} (parent {ppid} dead, {name})");
                    }
                }
            }
        }
    }
    Ok(report)
}

#[derive(Debug, Default)]
pub struct ReapReport { pub orphans_killed: u32, pub cleaned: u32 }

fn read_ppid(pid: u32) -> Option<u32> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("PPid:") { return rest.trim().parse().ok(); }
    }
    None
}
