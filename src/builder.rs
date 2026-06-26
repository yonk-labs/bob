use std::path::Path;
use std::time::Duration;
use tokio::process::Command;

pub trait Builder {
    async fn build(&self, prompt: &str, workdir: &Path) -> anyhow::Result<()>;
}

pub struct Opencode {
    pub cmd: String,
    pub timeout: Duration,
}

impl Builder for Opencode {
    async fn build(&self, prompt: &str, workdir: &Path) -> anyhow::Result<()> {
        // `--dir <workdir>` is REQUIRED, not just cosmetic: without it opencode
        // resolves its project root back to the main checkout of a git worktree
        // and edits the real tree instead of the isolated worktree, defeating
        // bob's isolation. `--dir` pins opencode inside the worktree.
        let mut child = Command::new(&self.cmd)
            .arg("run")
            .arg("--dir")
            .arg(workdir)
            .arg(prompt)
            .current_dir(workdir)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| anyhow::anyhow!("spawning builder '{}': {e}", self.cmd))?;
        match tokio::time::timeout(self.timeout, child.wait()).await {
            Ok(status) => {
                let status = status?;
                if !status.success() {
                    anyhow::bail!("builder exited with status {status}");
                }
                Ok(())
            }
            Err(_) => {
                if let Err(e) = child.start_kill() {
                    eprintln!("warning: failed to kill builder on timeout: {e}");
                }
                anyhow::bail!("builder timed out after {:?}", self.timeout);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn times_out_a_hung_builder() {
        // ShimSleep sleeps for 30s; the 200ms timeout must fire and kill it.
        let b = ShimSleep { secs: 30, timeout: Duration::from_millis(200) };
        let res = b.build("ignored", Path::new(".")).await;
        assert!(res.is_err(), "hung builder must time out");
    }

    // Test-only builder that sleeps, to exercise the timeout path without opencode.
    struct ShimSleep {
        secs: u64,
        timeout: Duration,
    }
    impl Builder for ShimSleep {
        async fn build(&self, _prompt: &str, _workdir: &Path) -> anyhow::Result<()> {
            let mut child = Command::new("sleep")
                .arg(self.secs.to_string())
                .kill_on_drop(true)
                .spawn()?;
            match tokio::time::timeout(self.timeout, child.wait()).await {
                Ok(s) => {
                    s?;
                    Ok(())
                }
                Err(_) => {
                    let _ = child.start_kill();
                    anyhow::bail!("timed out")
                }
            }
        }
    }
}
