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

pub struct Opencode {
    pub cmd: String,
    pub timeout: Duration,
    /// Extra args inserted before the prompt (e.g. ["--model", "provider/model"]).
    pub args: Vec<String>,
}

impl Builder for Opencode {
    async fn build(&self, prompt: &str, workdir: &Path) -> anyhow::Result<BuilderOutcome> {
        // `--dir <workdir>` is REQUIRED, not just cosmetic: without it opencode
        // resolves its project root back to the main checkout of a git worktree
        // and edits the real tree instead of the isolated worktree, defeating
        // bob's isolation. `--dir` pins opencode inside the worktree.
        let child = Command::new(&self.cmd)
            .arg("run")
            .arg("--dir")
            .arg(workdir)
            .args(&self.args)
            .arg(prompt)
            .current_dir(workdir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| anyhow::anyhow!("spawning builder '{}': {e}", self.cmd))?;
        match tokio::time::timeout(self.timeout, child.wait_with_output()).await {
            Ok(out) => {
                let out = out?;
                let stdout_tail = tail(&String::from_utf8_lossy(&out.stdout), 4000);
                let stderr_tail = tail(&String::from_utf8_lossy(&out.stderr), 4000);
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
            Err(_) => anyhow::bail!("builder timed out after {:?}", self.timeout),
        }
    }
}

pub fn tail(s: &str, max_chars: usize) -> String {
    let mut chars = s.chars().rev().take(max_chars).collect::<Vec<_>>();
    chars.reverse();
    chars.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn times_out_a_hung_builder() {
        // ShimSleep sleeps for 30s; the 200ms timeout must fire and kill it.
        let b = ShimSleep {
            secs: 30,
            timeout: Duration::from_millis(200),
        };
        let res = b.build("ignored", Path::new(".")).await;
        assert!(res.is_err(), "hung builder must time out");
    }

    // Test-only builder that sleeps, to exercise the timeout path without opencode.
    struct ShimSleep {
        secs: u64,
        timeout: Duration,
    }
    impl Builder for ShimSleep {
        async fn build(&self, _prompt: &str, _workdir: &Path) -> anyhow::Result<BuilderOutcome> {
            let mut child = Command::new("sleep")
                .arg(self.secs.to_string())
                .kill_on_drop(true)
                .spawn()?;
            match tokio::time::timeout(self.timeout, child.wait()).await {
                Ok(s) => {
                    s?;
                    Ok(BuilderOutcome {
                        failure_kind: "ok".into(),
                        ..Default::default()
                    })
                }
                Err(_) => {
                    let _ = child.start_kill();
                    anyhow::bail!("timed out")
                }
            }
        }
    }
}
