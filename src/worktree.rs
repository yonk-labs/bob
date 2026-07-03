use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Default)]
pub struct GcReport {
    pub dry_run: bool,
    pub worktrees: Vec<PathBuf>,
    pub branches: Vec<String>,
}

pub enum ApplyOutcome {
    Applied,
    BaseMoved,
}

pub struct Workspace {
    repo: PathBuf,
    dir: PathBuf,
    branch: String,
    base_sha: String,
}

fn git(args: &[&str], cwd: &Path) -> anyhow::Result<String> {
    let out = Command::new("git").args(args).current_dir(cwd).output()?;
    if !out.status.success() {
        anyhow::bail!(
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn git_stdout(args: &[&str], cwd: &Path) -> anyhow::Result<String> {
    let out = Command::new("git").args(args).current_dir(cwd).output()?;
    if !out.status.success() {
        anyhow::bail!(
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

fn bob_worktrees(repo: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let root = repo.join(".bob").join("worktrees");
    let mut out = Vec::new();
    if root.is_dir() {
        for entry in std::fs::read_dir(&root)? {
            let path = entry?.path();
            if path.is_dir() {
                out.push(path);
            }
        }
    }
    let listed = git(&["worktree", "list", "--porcelain"], repo).unwrap_or_default();
    for line in listed.lines().filter_map(|l| l.strip_prefix("worktree ")) {
        let path = PathBuf::from(line);
        if path.starts_with(&root) && !out.iter().any(|p| p == &path) {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

fn bob_branches(repo: &Path) -> anyhow::Result<Vec<String>> {
    let branches = git(&["branch", "--format=%(refname:short)"], repo)?;
    let mut out = branches
        .lines()
        .map(str::trim)
        .filter(|b| b.starts_with("bob/"))
        .map(str::to_string)
        .collect::<Vec<_>>();
    out.sort();
    Ok(out)
}

pub fn gc(dry_run: bool) -> anyhow::Result<GcReport> {
    let repo = std::env::current_dir()?;
    let worktrees = bob_worktrees(&repo)?;
    let branches = bob_branches(&repo)?;

    if !dry_run {
        for path in &worktrees {
            let path_str = path.to_string_lossy().to_string();
            if git(&["worktree", "remove", "--force", &path_str], &repo).is_err() && path.exists() {
                let _ = std::fs::remove_dir_all(path);
            }
        }
        let _ = git(&["worktree", "prune"], &repo);
        for branch in &branches {
            let _ = git(&["branch", "-D", branch], &repo);
        }
    }

    Ok(GcReport {
        dry_run,
        worktrees,
        branches,
    })
}

/// Apply `diff` to a FRESH detached worktree at `base_sha` and run the verify
/// gates there. This is the trust boundary for unattended apply: the reported
/// diff must reproduce a passing tree on its own, independent of whatever
/// state the build worktree accumulated. Err = diff didn't apply / git failed;
/// Ok(vr) with vr.passed=false = gates failed on the replayed tree.
pub fn replay_verify_at(
    repo: &Path,
    base_sha: &str,
    run_id: &str,
    diff: &str,
    cmds: &[String],
) -> anyhow::Result<crate::verify::VerifyResult> {
    let parent = repo.join(".bob").join("worktrees");
    std::fs::create_dir_all(&parent)?;
    let dir = parent.join(format!("{run_id}-replay"));
    let dir_str = dir.to_string_lossy().to_string();
    let _ = std::fs::remove_dir_all(&dir);
    let _ = git(&["worktree", "prune"], repo);
    git(&["worktree", "add", "--detach", &dir_str, base_sha], repo)?;
    let patch = parent.join(format!("{run_id}-replay.patch"));
    std::fs::write(&patch, diff)?;
    let patch_str = patch.to_string_lossy().to_string();
    let applied = git(&["apply", "--whitespace=nowarn", &patch_str], &dir);
    let result = match applied {
        Ok(_) => Ok(crate::verify::run_gates(cmds, &dir)),
        Err(e) => Err(anyhow::anyhow!(
            "final_diff does not apply cleanly to base {base_sha}: {e}"
        )),
    };
    let _ = git(&["worktree", "remove", "--force", &dir_str], repo);
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_file(&patch);
    result
}

impl Workspace {
    pub fn create(run_id: &str) -> anyhow::Result<Workspace> {
        let cwd = std::env::current_dir()?;
        let base_sha = git(&["rev-parse", "HEAD"], &cwd)?;
        let branch = format!("bob/{run_id}");
        // Place the worktree inside the repo under .bob/worktrees/<run_id> so the
        // opencode sandbox (which rejects /tmp/*) can operate on it.
        let wt_parent = cwd.join(".bob").join("worktrees");
        std::fs::create_dir_all(&wt_parent)?;
        let dir = wt_parent.join(run_id);
        // Remove any leftover directory from a prior run so `git worktree add` can create it fresh.
        let _ = std::fs::remove_dir_all(&dir);
        // Prune stale registrations so accumulated preserved worktrees don't block new ones.
        let _ = git(&["worktree", "prune"], &cwd);
        let dir_str = dir.to_string_lossy().to_string();
        git(
            &["worktree", "add", "-b", &branch, &dir_str, &base_sha],
            &cwd,
        )?;
        Ok(Workspace {
            repo: cwd,
            dir,
            branch,
            base_sha,
        })
    }

    pub fn path(&self) -> &Path {
        &self.dir
    }
    pub fn base_sha(&self) -> &str {
        &self.base_sha
    }

    /// Diff of all changes in the worktree vs base, including untracked files.
    pub fn capture_diff(&self) -> anyhow::Result<String> {
        git(&["add", "-A"], &self.dir)?; // stage incl. untracked
        git_stdout(
            &["diff", "--cached", "--no-renames", &self.base_sha],
            &self.dir,
        )
    }

    pub fn commit_candidate(&self, msg: &str) -> anyhow::Result<()> {
        git(&["add", "-A"], &self.dir)?;
        // allow empty so callers don't have to special-case no-op
        git(&["commit", "-q", "--allow-empty", "-m", msg], &self.dir)?;
        Ok(())
    }

    /// Apply the candidate commit to the main checkout only if HEAD is unchanged.
    ///
    /// Applies by checking out each changed path from the candidate commit (which
    /// force-overwrites the working tree) rather than `git cherry-pick`, so it is
    /// robust against untracked files in the main tree — e.g. a generated
    /// `Cargo.lock` — that cherry-pick would refuse to overwrite.
    pub fn apply_to_main(&self) -> anyhow::Result<ApplyOutcome> {
        let main = &self.repo;
        let current = git(&["rev-parse", "HEAD"], main)?;
        if current != self.base_sha {
            return Ok(ApplyOutcome::BaseMoved);
        }
        let candidate = git(&["rev-parse", "HEAD"], &self.dir)?;
        let status = git(
            &[
                "-c",
                "core.quotePath=false",
                "diff",
                "--no-renames",
                "--name-status",
                &self.base_sha,
                &candidate,
            ],
            main,
        )?;
        for line in status.lines() {
            let mut parts = line.splitn(2, '\t');
            let st = parts.next().unwrap_or("");
            let path = match parts.next() {
                Some(p) => p,
                None => continue,
            };
            match st.chars().next() {
                Some('D') => {
                    git(&["rm", "-q", "--", path], main)?;
                }
                Some(_) => {
                    git(&["checkout", &candidate, "--", path], main)?;
                }
                None => {}
            }
        }
        Ok(ApplyOutcome::Applied)
    }

    pub fn replay_verify(
        &self,
        run_id: &str,
        diff: &str,
        cmds: &[String],
    ) -> anyhow::Result<crate::verify::VerifyResult> {
        replay_verify_at(&self.repo, &self.base_sha, run_id, diff, cmds)
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
        let run = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .unwrap();
        };
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
        assert!(diff.ends_with('\n'), "artifact patches must apply cleanly");
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
        assert!(
            content.contains("changed"),
            "change landed in main checkout"
        );
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
        let run = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(&tmp)
                .output()
                .unwrap();
        };
        std::fs::write(tmp.join("b.txt"), "new\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-qm", "advance"]);

        let outcome = ws.apply_to_main().unwrap();
        ws.cleanup().unwrap();
        std::env::set_current_dir(prev).unwrap();

        assert!(
            matches!(outcome, ApplyOutcome::BaseMoved),
            "expected BaseMoved"
        );
    }

    #[test]
    fn apply_overwrites_untracked_collision() {
        // Candidate adds a file that already exists UNTRACKED in main (e.g. a
        // generated Cargo.lock). cherry-pick refused this; checkout-based apply must not.
        let _cwd_guard = crate::CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempdir_unique();
        init_repo(&tmp);
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&tmp).unwrap();

        let ws = Workspace::create("test4").unwrap();
        std::fs::write(ws.path().join("gen.lock"), "from-candidate\n").unwrap();
        ws.commit_candidate("adds gen.lock").unwrap();
        // main has an untracked gen.lock that would block a cherry-pick
        std::fs::write(tmp.join("gen.lock"), "stale-untracked\n").unwrap();

        let outcome = ws.apply_to_main().unwrap();
        ws.cleanup().unwrap();
        let content = std::fs::read_to_string(tmp.join("gen.lock")).unwrap();
        std::env::set_current_dir(prev).unwrap();

        assert!(
            matches!(outcome, ApplyOutcome::Applied),
            "expected Applied despite untracked collision"
        );
        assert!(
            content.contains("from-candidate"),
            "candidate version must overwrite the untracked file"
        );
    }

    #[test]
    fn gc_dry_run_reports_without_deleting() {
        let _cwd_guard = crate::CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempdir_unique();
        init_repo(&tmp);
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&tmp).unwrap();

        let ws = Workspace::create("gc-dry").unwrap();
        let report = gc(true).unwrap();
        assert!(report
            .worktrees
            .iter()
            .any(|p| p.ends_with(".bob/worktrees/gc-dry")));
        assert!(report.branches.contains(&"bob/gc-dry".to_string()));
        assert!(ws.path().exists(), "dry-run must not remove the worktree");

        ws.cleanup().unwrap();
        std::env::set_current_dir(prev).unwrap();
    }

    #[test]
    fn gc_removes_bob_worktree_and_branch() {
        let _cwd_guard = crate::CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempdir_unique();
        init_repo(&tmp);
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&tmp).unwrap();

        let ws = Workspace::create("gc-real").unwrap();
        let path = ws.path().to_path_buf();
        let report = gc(false).unwrap();
        assert!(report
            .worktrees
            .iter()
            .any(|p| p.ends_with(".bob/worktrees/gc-real")));
        assert!(!path.exists(), "gc must remove the worktree directory");
        let branches = git(&["branch", "--format=%(refname:short)"], &tmp).unwrap();
        assert!(!branches.lines().any(|b| b == "bob/gc-real"));

        std::env::set_current_dir(prev).unwrap();
    }

    fn sh(cmd: &str, cwd: &Path) {
        assert!(Command::new("sh").args(["-c", cmd]).current_dir(cwd).status().unwrap().success(), "{cmd}");
    }

    #[test]
    fn replay_verify_applies_diff_and_runs_gates() {
        let tmp = std::env::temp_dir().join(format!("bob-replay-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        sh("git init -q -b main && git -c user.email=t@t -c user.name=t commit -q --allow-empty -m init", &tmp);
        std::fs::write(tmp.join("a.txt"), "one\n").unwrap();
        sh("git add -A && git -c user.email=t@t -c user.name=t commit -q -m base", &tmp);
        let base = String::from_utf8(Command::new("git").args(["rev-parse", "HEAD"]).current_dir(&tmp).output().unwrap().stdout).unwrap().trim().to_string();
        // build a diff: modify a.txt and add b.txt
        std::fs::write(tmp.join("a.txt"), "two\n").unwrap();
        std::fs::write(tmp.join("b.txt"), "new\n").unwrap();
        sh("git add -A", &tmp);
        let diff = String::from_utf8(Command::new("git").args(["diff", "--cached", "--no-renames", &base]).current_dir(&tmp).output().unwrap().stdout).unwrap();
        sh("git reset -q --hard && git clean -qfd", &tmp);

        // gate that only passes if BOTH the modification and the new file landed
        let cmds = vec!["grep -q two a.txt && grep -q new b.txt".to_string()];
        let vr = replay_verify_at(&tmp, &base, "t1", &diff, &cmds).unwrap();
        assert!(vr.passed);

        // a gate that fails is reported as failed, not as an error
        let vr = replay_verify_at(&tmp, &base, "t2", &diff, &["false".to_string()]).unwrap();
        assert!(!vr.passed);

        // garbage diff is an error
        assert!(replay_verify_at(&tmp, &base, "t3", "not a diff", &cmds).is_err());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
