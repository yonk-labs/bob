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

/// Bounded retry-with-jitter wrapper around `git worktree add`. Two truly
/// concurrent `git worktree add` calls against one shared `.git` can hit
/// git's own internal metadata race (`fatal: failed to read
/// .git/worktrees/<sibling>/commondir`); it fails safely (nothing corrupts)
/// but self-clears once the peer's add finishes, so a short staggered retry
/// is enough. Retries on ANY add failure — a genuinely broken add still
/// fails all attempts and surfaces the real error.
///
/// A failed `-b <branch>` add can still leave the branch object behind (git
/// creates it while "Preparing worktree" before the race trips), which would
/// make a bare retry of the same command fail with "branch already exists"
/// instead of the transient race — so between attempts we defensively clear
/// any partial registration/branch this call itself may have left.
/// ponytail: bounded retry for git's concurrent worktree-add metadata race;
/// cross-process flock if 3 attempts ever proves too few.
fn worktree_add_with_retry(
    args: &[&str],
    cwd: &Path,
    dir: &Path,
    branch: Option<&str>,
) -> anyhow::Result<String> {
    const MAX_ATTEMPTS: u32 = 3;
    let mut last_err = None;
    for attempt in 0..MAX_ATTEMPTS {
        match git(args, cwd) {
            Ok(out) => return Ok(out),
            Err(e) => {
                last_err = Some(e);
                if attempt + 1 < MAX_ATTEMPTS {
                    let dir_str = dir.to_string_lossy().to_string();
                    let _ = git(&["worktree", "remove", "--force", &dir_str], cwd);
                    if let Some(b) = branch {
                        let _ = git(&["branch", "-D", b], cwd);
                    }
                    let nanos = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.subsec_nanos())
                        .unwrap_or(0);
                    let jitter = (nanos ^ std::process::id()) % 25;
                    let backoff_ms = u64::from(15 * (attempt + 1) + jitter);
                    std::thread::sleep(std::time::Duration::from_millis(backoff_ms));
                }
            }
        }
    }
    Err(last_err.unwrap())
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

/// Liveness marker path for a build worktree: a sibling `<run_id>.pid` under
/// `.bob/worktrees/` — deliberately OUTSIDE the worktree working tree, so it is
/// never staged by `capture_diff` (and, being a file not a dir, never mistaken
/// for a worktree by `bob_worktrees`). `Workspace::create` writes it; `gc` reads
/// it to avoid reclaiming a worktree whose build is still in flight.
fn worktree_pidfile(worktree: &Path) -> Option<PathBuf> {
    let name = worktree.file_name()?;
    Some(worktree.parent()?.join(format!("{}.pid", name.to_string_lossy())))
}

/// Is the build process that owns `worktree` still running? Reads the sibling
/// pidfile and checks `/proc/<pid>`.
/// ponytail: Linux-only /proc check (bob already assumes it — opencode sandbox).
/// On PID reuse the worst case is gc SKIPPING a stale worktree (safe, leaves
/// cruft) rather than deleting a live one — deliberately biased that way.
fn worktree_owner_alive(worktree: &Path) -> bool {
    worktree_pidfile(worktree)
        .filter(|p| p.exists())
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| s.trim().parse::<u32>().ok())
        .is_some_and(|pid| Path::new("/proc").join(pid.to_string()).exists())
}

pub fn gc(dry_run: bool) -> anyhow::Result<GcReport> {
    let repo = std::env::current_dir()?;
    // Never reclaim a worktree whose owning build is still alive: a `bob gc` run
    // DURING a live `--jobs N` campaign must not force-remove in-flight peers (it
    // would delete their worktree + branch wholesale). Partition first, act only
    // on the dead-owner ones.
    let (live, worktrees): (Vec<PathBuf>, Vec<PathBuf>) =
        bob_worktrees(&repo)?.into_iter().partition(|p| worktree_owner_alive(p));
    let live_ids: std::collections::HashSet<String> = live
        .iter()
        .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
        .collect();
    // Keep the branch of any live worktree; only report/delete reclaimable ones.
    let branches: Vec<String> = bob_branches(&repo)?
        .into_iter()
        .filter(|b| !live_ids.contains(b.strip_prefix("bob/").unwrap_or(b)))
        .collect();

    if !live.is_empty() {
        eprintln!(
            "bob gc: skipping {} in-use worktree(s) with a live build: {}",
            live.len(),
            live.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
        );
    }

    if !dry_run {
        for path in &worktrees {
            let path_str = path.to_string_lossy().to_string();
            if git(&["worktree", "remove", "--force", &path_str], &repo).is_err() && path.exists() {
                let _ = std::fs::remove_dir_all(path);
            }
            if let Some(pidfile) = worktree_pidfile(path) {
                let _ = std::fs::remove_file(pidfile);
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

/// Run `worktree.setup_cmds` once, in order, in a freshly created worktree —
/// before iteration 0 / before replay verify, never in the main tree. Uses the
/// same `sh -c` shell-exec mechanism as verify gates (see verify::run_gates),
/// with cwd = the new worktree and `BOB_REPO_ROOT` exported to the main repo
/// root. A failing cmd (non-zero exit) is an INFRA error, not a gate failure:
/// it returns `Err` naming the command and its stderr, distinct from
/// `verify::run_gates`'s `Ok(VerifyResult{passed: false, ..})`, so callers
/// never mistake it for a builder/task/judge failure.
fn run_setup_cmds(cmds: &[String], workdir: &Path, repo_root: &Path) -> anyhow::Result<()> {
    for cmd in cmds {
        let out = Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .current_dir(workdir)
            .env("BOB_REPO_ROOT", repo_root)
            .output()
            .map_err(|e| anyhow::anyhow!("worktree setup cmd '{cmd}' could not run: {e}"))?;
        if !out.status.success() {
            anyhow::bail!(
                "worktree setup cmd failed: {cmd}\n--- stderr ---\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
    Ok(())
}

/// Apply `diff` to a FRESH detached worktree at `base_sha`, run
/// `worktree.setup_cmds` there (see [`run_setup_cmds`] for the infra-error
/// contract), then run the verify gates. This is the trust boundary for
/// unattended apply: the reported diff must reproduce a passing tree on its
/// own, independent of whatever state the build worktree accumulated.
/// Err = setup cmd failed / diff didn't apply / git failed;
/// Ok(vr) with vr.passed=false = gates failed on the replayed tree.
pub fn replay_verify_at_with_setup(
    repo: &Path,
    base_sha: &str,
    run_id: &str,
    diff: &str,
    cmds: &[String],
    setup_cmds: &[String],
) -> anyhow::Result<crate::verify::VerifyResult> {
    let parent = repo.join(".bob").join("worktrees");
    std::fs::create_dir_all(&parent)?;
    let dir = parent.join(format!("{run_id}-replay"));
    let dir_str = dir.to_string_lossy().to_string();
    // Scoped teardown of only this replay worktree — never a blanket
    // `git worktree prune`, which would corrupt a concurrent peer build's
    // registration (see Workspace::create / bug #20).
    let _ = git(&["worktree", "remove", "--force", &dir_str], repo);
    let _ = std::fs::remove_dir_all(&dir);
    worktree_add_with_retry(
        &["worktree", "add", "--detach", &dir_str, base_sha],
        repo,
        &dir,
        None,
    )?;
    if let Err(e) = run_setup_cmds(setup_cmds, &dir, repo) {
        let _ = git(&["worktree", "remove", "--force", &dir_str], repo);
        let _ = std::fs::remove_dir_all(&dir);
        return Err(e);
    }
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
    /// Create a fresh build worktree, then run `setup_cmds` (worktree.setup_cmds)
    /// once, in order, before returning it — before iteration 0, never in the
    /// main tree. See [`run_setup_cmds`] for the infra-error contract.
    pub fn create(run_id: &str, setup_cmds: &[String]) -> anyhow::Result<Workspace> {
        let cwd = std::env::current_dir()?;
        let base_sha = git(&["rev-parse", "HEAD"], &cwd)?;
        let branch = format!("bob/{run_id}");
        // Place the worktree inside the repo under .bob/worktrees/<run_id> so the
        // opencode sandbox (which rejects /tmp/*) can operate on it.
        let wt_parent = cwd.join(".bob").join("worktrees");
        std::fs::create_dir_all(&wt_parent)?;
        let dir = wt_parent.join(run_id);
        let dir_str = dir.to_string_lossy().to_string();
        // Tear down only THIS run's leftover worktree so a rerun with the same
        // run_id can recreate it — scoped to our own dir, NEVER a blanket
        // `git worktree prune`. A blanket prune mutates the shared worktree
        // registry that concurrent peer builds (hector `dispatch --jobs N`, one
        // shared .git) depend on; pruning a peer's registration deregisters its
        // worktree, so the peer's `git add -A` resolves to the MAIN repo and
        // stages sibling worktrees into it (bug #20). `worktree remove --force`
        // deregisters + deletes our leftover if present (live or prunable);
        // `remove_dir_all` then clears any stray dir with no live registration.
        let _ = git(&["worktree", "remove", "--force", &dir_str], &cwd);
        let _ = std::fs::remove_dir_all(&dir);
        worktree_add_with_retry(
            &["worktree", "add", "-b", &branch, &dir_str, &base_sha],
            &cwd,
            &dir,
            Some(&branch),
        )?;
        // On setup failure, remove the worktree AND the bob/<run_id> branch —
        // a fresh checkout that never ran a builder has nothing worth keeping
        // (the failing cmd's stderr is already in the error), and leaked
        // branches otherwise accumulate until `bob gc`. Mirrors the replay
        // path's cleanup above.
        if let Err(e) = run_setup_cmds(setup_cmds, &dir, &cwd) {
            let _ = git(&["worktree", "remove", "--force", &dir_str], &cwd);
            let _ = std::fs::remove_dir_all(&dir);
            let _ = git(&["branch", "-D", &branch], &cwd);
            return Err(e);
        }
        // Mark this worktree live so `bob gc` won't reclaim it mid-build (see
        // worktree_owner_alive). Best-effort; a missing marker just means gc
        // treats it as reclaimable, matching pre-liveness behavior.
        if let Some(pidfile) = worktree_pidfile(&dir) {
            let _ = std::fs::write(pidfile, std::process::id().to_string());
        }
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
        // Stage all worktree changes (incl. untracked) but NEVER the repo's
        // `.bob/` tree (where sibling build worktrees live). The `,top` anchors
        // the exclude at the repo root, so even if this worktree's git
        // resolution has fallen back to the MAIN repo — e.g. its registration
        // was pruned by a concurrent build — `git add -A` cannot stage another
        // build's files or pollute the main index (bug #20 / #21).
        git(&["add", "-A", "--", ".", ":(exclude,top).bob"], &self.dir)?;
        git_stdout(
            &["diff", "--cached", "--no-renames", &self.base_sha],
            &self.dir,
        )
    }

    pub fn commit_candidate(&self, msg: &str) -> anyhow::Result<()> {
        // Exclude `.bob/` for the same reason as capture_diff — a candidate
        // commit must never absorb sibling build worktrees (bug #20 / #21).
        git(&["add", "-A", "--", ".", ":(exclude,top).bob"], &self.dir)?;
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
        setup_cmds: &[String],
    ) -> anyhow::Result<crate::verify::VerifyResult> {
        replay_verify_at_with_setup(&self.repo, &self.base_sha, run_id, diff, cmds, setup_cmds)
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

        let ws = Workspace::create("test1", &[]).unwrap();
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

        let ws = Workspace::create("test2", &[]).unwrap();
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

        let ws = Workspace::create("test3", &[]).unwrap();
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

        let ws = Workspace::create("test4", &[]).unwrap();
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

        let ws = Workspace::create("gc-dry", &[]).unwrap();
        // Simulate a FINISHED build (owner process exited) so gc treats it as
        // reclaimable; create() writes our own live pid, which gc would skip.
        std::fs::write(worktree_pidfile(ws.path()).unwrap(), u32::MAX.to_string()).unwrap();
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

        let ws = Workspace::create("gc-real", &[]).unwrap();
        let path = ws.path().to_path_buf();
        // Finished build → dead owner pid, so gc reclaims it.
        std::fs::write(worktree_pidfile(&path).unwrap(), u32::MAX.to_string()).unwrap();
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

    #[test]
    fn gc_skips_worktree_with_live_owner() {
        // bob gc during a live --jobs N campaign must NOT reclaim an in-flight
        // peer. create() writes our own (alive) pid as the owner, so gc must
        // leave the worktree + branch untouched and not report it as reclaimed.
        let _cwd_guard = crate::CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempdir_unique();
        init_repo(&tmp);
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&tmp).unwrap();

        let ws = Workspace::create("gc-live", &[]).unwrap();
        let report = gc(false).unwrap();
        assert!(ws.path().exists(), "gc must not remove a worktree with a live owner");
        assert!(
            !report.worktrees.iter().any(|p| p.ends_with(".bob/worktrees/gc-live")),
            "live worktree must not be reported as reclaimed"
        );
        let branches = git(&["branch", "--format=%(refname:short)"], &tmp).unwrap();
        assert!(branches.lines().any(|b| b == "bob/gc-live"), "live branch must survive");

        ws.cleanup().unwrap();
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
        let vr = replay_verify_at_with_setup(&tmp, &base, "t1", &diff, &cmds, &[]).unwrap();
        assert!(vr.passed);

        // a gate that fails is reported as failed, not as an error
        let vr = replay_verify_at_with_setup(&tmp, &base, "t2", &diff, &["false".to_string()], &[]).unwrap();
        assert!(!vr.passed);

        // garbage diff is an error
        assert!(replay_verify_at_with_setup(&tmp, &base, "t3", "not a diff", &cmds, &[]).is_err());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn create_runs_setup_cmds_in_fresh_worktree_with_repo_root_env() {
        let _cwd_guard = crate::CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempdir_unique();
        init_repo(&tmp);
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&tmp).unwrap();

        let setup_cmds = vec![
            "echo first > order.txt".to_string(),
            "echo second >> order.txt".to_string(),
            "echo \"$BOB_REPO_ROOT\" > root.txt".to_string(),
        ];
        let ws = Workspace::create("setup1", &setup_cmds).unwrap();

        let order = std::fs::read_to_string(ws.path().join("order.txt")).unwrap();
        assert_eq!(order, "first\nsecond\n", "setup cmds ran in order, once");
        let root = std::fs::read_to_string(ws.path().join("root.txt")).unwrap();
        assert_eq!(
            root.trim(),
            tmp.canonicalize().unwrap().to_string_lossy(),
            "BOB_REPO_ROOT points at the main repo root, not the worktree"
        );

        ws.cleanup().unwrap();
        std::env::set_current_dir(prev).unwrap();
    }

    #[test]
    fn create_fails_fast_with_infra_error_on_bad_setup_cmd() {
        let _cwd_guard = crate::CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempdir_unique();
        init_repo(&tmp);
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&tmp).unwrap();

        let setup_cmds = vec!["echo boom-stderr 1>&2 && exit 3".to_string()];
        let msg = match Workspace::create("setup-fail", &setup_cmds) {
            Ok(_) => panic!("expected setup cmd failure to abort worktree creation"),
            Err(e) => e.to_string(),
        };
        assert!(
            msg.contains("echo boom-stderr 1>&2 && exit 3"),
            "error names the failing cmd: {msg}"
        );
        assert!(msg.contains("boom-stderr"), "error carries stderr: {msg}");
        // No leaked artifacts: the fresh worktree and its bob/<run_id> branch
        // are removed (a checkout that never ran a builder has no value).
        assert!(
            !tmp.join(".bob").join("worktrees").join("setup-fail").exists(),
            "worktree removed on setup failure"
        );
        let branches = Command::new("git")
            .args(["branch", "--list", "bob/setup-fail"])
            .current_dir(&tmp)
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&branches.stdout).trim().is_empty(),
            "bob/setup-fail branch removed on setup failure"
        );

        std::env::set_current_dir(prev).unwrap();
    }

    #[test]
    fn replay_verify_at_with_setup_runs_setup_cmds_before_gates() {
        let tmp = std::env::temp_dir().join(format!("bob-replay-setup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        sh("git init -q -b main && git -c user.email=t@t -c user.name=t commit -q --allow-empty -m init", &tmp);
        std::fs::write(tmp.join("a.txt"), "one\n").unwrap();
        sh("git add -A && git -c user.email=t@t -c user.name=t commit -q -m base", &tmp);
        let base = String::from_utf8(Command::new("git").args(["rev-parse", "HEAD"]).current_dir(&tmp).output().unwrap().stdout).unwrap().trim().to_string();
        std::fs::write(tmp.join("a.txt"), "two\n").unwrap();
        sh("git add -A", &tmp);
        let diff = String::from_utf8(Command::new("git").args(["diff", "--cached", "--no-renames", &base]).current_dir(&tmp).output().unwrap().stdout).unwrap();
        sh("git reset -q --hard && git clean -qfd", &tmp);

        // The gate checks BOTH the diff landed AND the file the setup cmd created —
        // proving setup cmds ran ahead of the patch apply / gate.
        let setup_cmds = vec!["echo ready > setup-marker.txt".to_string()];
        let cmds = vec!["grep -q two a.txt && test -f setup-marker.txt".to_string()];
        let vr = replay_verify_at_with_setup(&tmp, &base, "s1", &diff, &cmds, &setup_cmds).unwrap();
        assert!(vr.passed, "gate sees the file the setup cmd created: {}", vr.output);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn capture_diff_never_stages_sibling_worktrees_into_main() {
        // Deterministic repro for bug #20/#21. Worktrees live UNDER the repo at
        // .bob/worktrees/<id>. If a build's worktree registration is lost — e.g.
        // a concurrent build's blanket `git worktree prune` deregistered it — the
        // worktree's `.git` link no longer resolves, git walks UP to the MAIN
        // repo, and an unscoped `git add -A` stages EVERY sibling build's
        // worktree into main. That cross-contaminates the captured diff (→
        // ScopeExceeded) and dirties the main tree/index despite apply=false.
        // Here we model that lost registration directly (remove the worktree's
        // `.git` link) so the repro is deterministic rather than timing-dependent.
        let _cwd_guard = crate::CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempdir_unique();
        init_repo(&tmp);
        // NOTE: .bob deliberately NOT gitignored — this is the vulnerable repo
        // shape where sibling worktrees are stageable from the main root.
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&tmp).unwrap();

        let ws_a = Workspace::create("bldA", &[]).unwrap();
        let ws_b = Workspace::create("bldB", &[]).unwrap();
        std::fs::write(ws_b.path().join("peer_b.txt"), "from build B\n").unwrap();
        std::fs::write(ws_a.path().join("mine_a.txt"), "from build A\n").unwrap();

        // Simulate the lost registration a concurrent prune produces: drop A's
        // `.git` link so A's git resolution falls back to the MAIN repo.
        std::fs::remove_file(ws_a.path().join(".git")).unwrap();

        let diff = ws_a.capture_diff().unwrap();

        let staged = git(&["diff", "--cached", "--name-only"], &tmp).unwrap();
        // Clean up before asserting so a failure never leaks worktrees/branches.
        let _ = ws_a.cleanup();
        let _ = ws_b.cleanup();
        let _ = git(&["reset", "-q", "--hard"], &tmp);
        let _ = git(&["clean", "-qfd"], &tmp);
        std::env::set_current_dir(prev).unwrap();

        assert!(
            !diff.contains("peer_b.txt"),
            "build A's captured diff leaked peer build B's file:\n{diff}"
        );
        assert!(
            staged.trim().is_empty(),
            "main index was polluted by a build's `git add -A`:\n{staged}"
        );
    }

    #[test]
    fn concurrent_builds_stay_isolated() {
        // Regression guard for bug #20: two bob builds share ONE main-repo .git
        // (hector `dispatch --jobs 2`). Each build creates its own worktree, edits
        // a DISJOINT file, and captures its diff — concurrently. Each diff must
        // contain ONLY its own file, and the main tree must stay clean. Before the
        // fix, each create's blanket `git worktree prune` could rmdir the shared
        // `.git/worktrees` out from under the peer's `git worktree add` (add fails
        // with "Invalid path '.git/worktrees'") or deregister the peer's worktree
        // (its `git add -A` then resolves to main and stages the other build).
        use std::thread;

        let _cwd_guard = crate::CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempdir_unique();
        init_repo(&tmp);
        std::fs::write(tmp.join(".gitignore"), "/.bob\n").unwrap();
        {
            let run = |args: &[&str]| {
                Command::new("git").args(args).current_dir(&tmp).output().unwrap();
            };
            run(&["add", ".gitignore"]);
            run(&["commit", "-qm", "gitignore .bob"]);
        }
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&tmp).unwrap();

        let mut failures: Vec<String> = Vec::new();
        for i in 0..25 {
            // Two builds started as close to simultaneously as possible, sharing
            // the one repo cwd — exactly the `--jobs 2` shape.
            let spawn_one = |run_id: String, fname: String| {
                thread::spawn(move || -> Result<String, String> {
                    let ws = Workspace::create(&run_id, &[])
                        .map_err(|e| format!("{run_id}: create failed: {e}"))?;
                    std::fs::write(ws.path().join(&fname), "x\n").unwrap();
                    let diff = ws
                        .capture_diff()
                        .map_err(|e| format!("{run_id}: capture_diff failed: {e}"))?;
                    let out = if !diff.contains(&fname) {
                        Err(format!("{run_id}: own file {fname} missing from diff"))
                    } else {
                        Ok(diff)
                    };
                    let _ = ws.cleanup();
                    out
                })
            };
            let fa = format!("only_a_{i}.txt");
            let fb = format!("only_b_{i}.txt");
            let ha = spawn_one(format!("cc-a-{i}"), fa.clone());
            let hb = spawn_one(format!("cc-b-{i}"), fb.clone());
            match ha.join().unwrap() {
                Ok(diff) if diff.contains(&fb) => {
                    failures.push(format!("iter {i}: build A's diff leaked peer file {fb}"))
                }
                Ok(_) => {}
                Err(e) => failures.push(format!("iter {i}: {e}")),
            }
            match hb.join().unwrap() {
                Ok(diff) if diff.contains(&fa) => {
                    failures.push(format!("iter {i}: build B's diff leaked peer file {fa}"))
                }
                Ok(_) => {}
                Err(e) => failures.push(format!("iter {i}: {e}")),
            }
            let status = git(&["status", "--porcelain"], &tmp).unwrap();
            if !status.trim().is_empty() {
                failures.push(format!("iter {i}: main tree dirty:\n{status}"));
            }
            let _ = git(&["reset", "-q", "--hard"], &tmp);
            let _ = git(&["clean", "-qfd"], &tmp);
        }

        std::env::set_current_dir(prev).unwrap();
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn replay_verify_at_with_setup_cmd_failure_is_infra_error_not_a_failed_gate() {
        let tmp = std::env::temp_dir().join(format!("bob-replay-setup-fail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        sh("git init -q -b main && git -c user.email=t@t -c user.name=t commit -q --allow-empty -m init", &tmp);
        std::fs::write(tmp.join("a.txt"), "one\n").unwrap();
        sh("git add -A && git -c user.email=t@t -c user.name=t commit -q -m base", &tmp);
        let base = String::from_utf8(Command::new("git").args(["rev-parse", "HEAD"]).current_dir(&tmp).output().unwrap().stdout).unwrap().trim().to_string();

        let setup_cmds = vec!["echo setup-boom 1>&2 && exit 9".to_string()];
        // A gate that would pass, to prove the failure is the setup cmd's, not the gate's.
        let cmds = vec!["true".to_string()];
        let err = replay_verify_at_with_setup(&tmp, &base, "s2", "", &cmds, &setup_cmds).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("setup-boom"), "error surfaces setup cmd stderr: {msg}");

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
