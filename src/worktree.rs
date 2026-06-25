use std::path::{Path, PathBuf};
use std::process::Command;

pub enum ApplyOutcome { Applied, BaseMoved }

pub struct Workspace {
    repo: PathBuf,
    dir: PathBuf,
    branch: String,
    base_sha: String,
}

fn git(args: &[&str], cwd: &Path) -> anyhow::Result<String> {
    let out = Command::new("git").args(args).current_dir(cwd).output()?;
    if !out.status.success() {
        anyhow::bail!("git {:?} failed: {}", args, String::from_utf8_lossy(&out.stderr));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

impl Workspace {
    pub fn create(run_id: &str) -> anyhow::Result<Workspace> {
        let cwd = std::env::current_dir()?;
        let base_sha = git(&["rev-parse", "HEAD"], &cwd)?;
        let branch = format!("bob/{run_id}");
        let dir = std::env::temp_dir().join(format!("bob-wt-{run_id}"));
        // Remove any leftover directory from a prior run so `git worktree add` can create it fresh.
        let _ = std::fs::remove_dir_all(&dir);
        let dir_str = dir.to_string_lossy().to_string();
        git(&["worktree", "add", "-b", &branch, &dir_str, &base_sha], &cwd)?;
        Ok(Workspace { repo: cwd, dir, branch, base_sha })
    }

    pub fn path(&self) -> &Path { &self.dir }
    pub fn base_sha(&self) -> &str { &self.base_sha }

    /// Diff of all changes in the worktree vs base, including untracked files.
    pub fn capture_diff(&self) -> anyhow::Result<String> {
        git(&["add", "-A"], &self.dir)?;            // stage incl. untracked
        git(&["diff", "--cached", &self.base_sha], &self.dir)
    }

    pub fn commit_candidate(&self, msg: &str) -> anyhow::Result<()> {
        git(&["add", "-A"], &self.dir)?;
        // allow empty so callers don't have to special-case no-op
        git(&["commit", "-q", "--allow-empty", "-m", msg], &self.dir)?;
        Ok(())
    }

    /// Apply the candidate commit to the main checkout only if HEAD is unchanged.
    pub fn apply_to_main(&self) -> anyhow::Result<ApplyOutcome> {
        let main = &self.repo;
        let current = git(&["rev-parse", "HEAD"], main)?;
        if current != self.base_sha {
            return Ok(ApplyOutcome::BaseMoved);
        }
        let candidate = git(&["rev-parse", "HEAD"], &self.dir)?;
        git(&["cherry-pick", "--no-commit", &candidate], &main)?;
        Ok(ApplyOutcome::Applied)
    }

    pub fn cleanup(&self) -> anyhow::Result<()> {
        let dir_str = self.dir.to_string_lossy().to_string();
        let _ = git(&["worktree", "remove", "--force", &dir_str], &self.repo);
        let _ = git(&["branch", "-D", &self.branch], &self.repo);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static TEST_COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn init_repo(dir: &std::path::Path) {
        let run = |args: &[&str]| { Command::new("git").args(args).current_dir(dir).output().unwrap(); };
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(dir.join("a.txt"), "hello\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-qm", "init"]);
    }

    fn tempdir_unique() -> std::path::PathBuf {
        let n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!("bob-test-{}-{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn captures_diff_including_untracked() {
        let _cwd_guard = crate::CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempdir_unique();
        init_repo(&tmp);
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&tmp).unwrap();

        let ws = Workspace::create("test1").unwrap();
        // simulate the builder editing in the worktree
        std::fs::write(ws.path().join("a.txt"), "hello\nworld\n").unwrap();
        std::fs::write(ws.path().join("new.txt"), "created\n").unwrap();
        let diff = ws.capture_diff().unwrap();

        ws.cleanup().unwrap(); // prevent orphan worktree/branch between runs
        std::env::set_current_dir(prev).unwrap();
        assert!(diff.contains("world"), "modified file in diff");
        assert!(diff.contains("new.txt"), "untracked file in diff");
    }

    #[test]
    fn apply_to_main_applies_when_base_unchanged() {
        let _cwd_guard = crate::CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempdir_unique();
        init_repo(&tmp);
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&tmp).unwrap();

        let ws = Workspace::create("test2").unwrap();
        std::fs::write(ws.path().join("a.txt"), "changed\n").unwrap();
        ws.commit_candidate("test change").unwrap();

        let outcome = ws.apply_to_main().unwrap();
        ws.cleanup().unwrap();
        let content = std::fs::read_to_string(tmp.join("a.txt")).unwrap();
        std::env::set_current_dir(prev).unwrap();

        assert!(matches!(outcome, ApplyOutcome::Applied), "expected Applied");
        assert!(content.contains("changed"), "change landed in main checkout");
    }

    #[test]
    fn apply_to_main_detects_base_moved() {
        let _cwd_guard = crate::CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempdir_unique();
        init_repo(&tmp);
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&tmp).unwrap();

        let ws = Workspace::create("test3").unwrap();
        ws.commit_candidate("candidate").unwrap();

        // Advance main HEAD after Workspace::create
        let run = |args: &[&str]| { Command::new("git").args(args).current_dir(&tmp).output().unwrap(); };
        std::fs::write(tmp.join("b.txt"), "new\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-qm", "advance"]);

        let outcome = ws.apply_to_main().unwrap();
        ws.cleanup().unwrap();
        std::env::set_current_dir(prev).unwrap();

        assert!(matches!(outcome, ApplyOutcome::BaseMoved), "expected BaseMoved");
    }
}
