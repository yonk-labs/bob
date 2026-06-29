use std::path::Path;
use std::time::Duration;
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
pub struct GooseBuilder {
    pub cmd: String,
    pub model: String,
    pub timeout: Duration,
    pub provider: String,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
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

        // Set OPENAI_BASE_URL / OPENAI_API_KEY for local endpoints
        if let Some(url) = &self.base_url {
            cmd.env("OPENAI_BASE_URL", url);
            cmd.env("OPENAI_API_KEY", self.api_key.as_deref().unwrap_or("local"));
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

        match tokio::time::timeout(self.timeout, child.wait_with_output()).await {
            Ok(out) => {
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
                Ok(BuilderOutcome {
                    stdout_tail,
                    stderr_tail,
                    failure_kind: "ok".into(),
                })
            }
            Err(_) => anyhow::bail!("goose timed out after {:?}", self.timeout),
        }
    }
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
                if let Some(pid) = child_pid {
                    let _ = unsafe { libc::kill(pid as i32, libc::SIGTERM) };
                    std::thread::sleep(Duration::from_millis(200));
                    let _ = unsafe { libc::kill(pid as i32, libc::SIGKILL) };
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
            let pid_file = entry.path().join("opencode.pid");
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
                    let _ = unsafe { libc::kill(pid as i32, libc::SIGTERM) };
                    std::thread::sleep(Duration::from_millis(200));
                    let _ = unsafe { libc::kill(pid as i32, libc::SIGKILL) };
                    let _ = std::fs::remove_file(&pid_file);
                    report.orphans_killed += 1;
                    eprintln!("reaper: killed orphan opencode pid={pid} (parent {ppid} dead)");
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
