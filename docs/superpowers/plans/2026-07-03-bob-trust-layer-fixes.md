# Bob Trust-Layer Fixes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make bob's reported diff trustworthy for unattended apply — fix the test-freeze policy that silently ate the pilot's diff, empty judge critiques, and missing replay verification; add structured run telemetry.

**Architecture:** Nine thin slices against the existing build→verify→judge loop in `src/engine.rs`. No new crates, no new modules — each slice extends an existing file. All RunResult JSON changes are additive (hector routes on the existing strings; see the cross-repo contract test in `src/report.rs:144`).

**Tech Stack:** Rust (edition per Cargo.toml), anyhow, serde/serde_json/serde_yaml, clap. Tests via `cargo test` (98 passing at baseline).

## Global Constraints

- Work on branch `trust-layer` (created in Task 1, Step 0). One commit per task, conventional-commit style (`fix(engine): ...`).
- NEVER rename existing `RunStatus` / `NextAction` / `StopReason` variants or their emitted strings — hector parses them (`src/report.rs:139-157`). New variants are allowed.
- All new RunResult JSON fields are additive; never remove or rename existing fields.
- `cargo test` must pass at the end of every task. Run it before committing.
- No new dependencies.
- Adding a field to `RunResult` (Tasks 3, 6, 7) breaks every struct-literal constructor in tests (`src/report.rs` tests, `src/engine.rs` tests). Fix them all by adding the field with an empty/zero default — `cargo build` lists each site.
- Repo root: `/home/yonk/yonk-tools/bob`. All paths below are relative to it.

---

### Task 1: Judge critique falls back to `take` prose

**Files:**
- Modify: `src/judge.rs:94-102`
- Test: `src/judge.rs` (inline `#[cfg(test)] mod tests`)

**Interfaces:**
- Produces: unchanged signature `parse_abe_validate(&str) -> anyhow::Result<JudgeOutcome>`; behavior change only — a structured verdict with empty `disagreements` now uses the `take` prose as critique.

**Why:** abe can return `{"verdict":"fail","take":"<prose>","disagreements":[]}`. Today the critique comes only from `collect_disagreements`, so the caller gets `fail` + `""` — a blocking fail with nothing to feed the retry (the pilot's exact failure). The `take` fallback at line 107 is only reached when there is *no* `verdict` field.

- [ ] **Step 0: Create the branch**

```bash
cd /home/yonk/yonk-tools/bob && git checkout -b trust-layer
```

- [ ] **Step 1: Write the failing test** (add inside the existing `mod tests` in `src/judge.rs`)

```rust
#[test]
fn fail_with_empty_disagreements_falls_back_to_take() {
    let j = r#"{"verdict":"fail","take":"the diff duplicates the slow tests","disagreements":[]}"#;
    let o = parse_abe_validate(j).unwrap();
    assert!(matches!(o.verdict, Verdict::Fail));
    assert_eq!(o.critique, "the diff duplicates the slow tests");
}

#[test]
fn fail_with_disagreements_ignores_take() {
    let j = r#"{"verdict":"fail","take":"prose","disagreements":["off-by-one in loop"]}"#;
    let o = parse_abe_validate(j).unwrap();
    assert!(o.critique.contains("off-by-one"));
    assert!(!o.critique.contains("prose"));
}
```

- [ ] **Step 2: Run to verify the first test fails**

Run: `cargo test fail_with_empty_disagreements_falls_back_to_take`
Expected: FAIL — `assertion failed` (critique is `""`).

- [ ] **Step 3: Implement.** In `parse_abe_validate`, replace the verdict branch body (currently lines 100-101):

```rust
        let mut critique = collect_disagreements(&v);
        if critique.trim().is_empty() {
            if let Some(take) = v.get("take").and_then(|x| x.as_str()) {
                critique = take.to_string();
            }
        }
        return Ok(JudgeOutcome { verdict, critique });
```

- [ ] **Step 4: Run the full suite**

Run: `cargo test`
Expected: all pass (baseline 98 + 2 new).

- [ ] **Step 5: Commit**

```bash
git add src/judge.rs && git commit -m "fix(judge): fall back to abe 'take' prose when disagreements are empty"
```

---

### Task 2: Keep the worktree on any non-converged outcome

**Files:**
- Modify: `src/engine.rs:1007-1015` (builder-error early return) and `src/engine.rs:1196-1200` (end-of-run cleanup)
- Test: `src/engine.rs` (inline tests)

**Interfaces:**
- Produces: `fn should_keep_worktree(keep_flag: bool, status: RunStatus) -> bool` (private, in `engine.rs`) — used at the end-of-run cleanup site.

**Why:** Today cleanup is unconditional unless `--keep`. In the pilot, the worktree was the only correct artifact of a non-converged run. `bob gc` already exists to reap accumulated worktrees.

- [ ] **Step 1: Write the failing test** (in `src/engine.rs` `mod tests`)

```rust
#[test]
fn worktree_kept_on_non_converged() {
    assert!(should_keep_worktree(false, RunStatus::NotConverged));
    assert!(should_keep_worktree(false, RunStatus::NeedsReview));
    assert!(should_keep_worktree(false, RunStatus::Error));
    assert!(!should_keep_worktree(false, RunStatus::Converged));
    assert!(should_keep_worktree(true, RunStatus::Converged));
}
```

- [ ] **Step 2: Run to verify it fails to compile** (`should_keep_worktree` undefined)

Run: `cargo test worktree_kept_on_non_converged`

- [ ] **Step 3: Implement.** Add near `verdict_name` in `engine.rs`:

```rust
/// A non-converged worktree is often the only correct artifact of the run
/// (pilot lesson: the diff can be wrong while the tree is right). Keep it;
/// `bob gc` reaps them.
fn should_keep_worktree(keep_flag: bool, status: RunStatus) -> bool {
    keep_flag || status != RunStatus::Converged
}
```

Replace the end-of-run cleanup (currently `if opts.keep_worktree { ... } else { ws.cleanup()?; }`):

```rust
    if should_keep_worktree(opts.keep_worktree, status) {
        eprintln!("worktree preserved at {}", ws.path().display());
    } else {
        ws.cleanup()?;
    }
```

Replace the builder-error early return (currently cleans up unless `keep_worktree`):

```rust
            Err(e) => {
                eprintln!(
                    "bob: builder error — worktree preserved at {}",
                    ws.path().display()
                );
                return Err(e);
            }
```

- [ ] **Step 4: Run the full suite**

Run: `cargo test` — expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add src/engine.rs && git commit -m "fix(engine): preserve worktree on any non-converged outcome"
```

---

### Task 3: `editable_paths` exempt test freeze/reset; surface resets in RunResult

**Files:**
- Modify: `src/engine.rs` (`reset_test_files`, its call site ~line 1023, `build_prompt`, `RunResult`, run() result construction)
- Modify: `src/report.rs` (`to_json` + fix test constructors)
- Test: `src/engine.rs` inline tests

**Interfaces:**
- Produces: `fn reset_test_files(workdir: &Path, base_sha: &str, editable_paths: &[String]) -> Vec<String>` — returns the paths actually reset.
- Produces: new `RunResult` field `pub reset_test_files: Vec<String>` and JSON key `"reset_test_files"` (additive).
- Produces: `fn path_allowed(path: &str, allow: &[String]) -> bool` (private) — boundary-aware prefix match, same semantics as `src/scope.rs:34-37`.

**Why (root cause of the pilot):** `reset_test_files` unconditionally reverts any change to test files — even when the caller explicitly allow-listed the test file. The pilot's task ("split a test suite") was sabotaged by design, and the only trace was one stderr line. Fix: files under `editable_paths` are exempt from the reset; every reset that does happen is reported as structured data. `freeze_untracked_test_files` stays unchanged (freezing an editable test into base is harmless — the diff still carries the builder's changes).

- [ ] **Step 1: Write the failing tests** (in `src/engine.rs` `mod tests`)

```rust
#[test]
fn path_allowed_is_boundary_aware() {
    let allow = vec!["core/".to_string(), "src".to_string()];
    assert!(path_allowed("core/ai.test.ts", &allow));
    assert!(path_allowed("src/x.rs", &allow));
    assert!(path_allowed("src", &allow));
    assert!(!path_allowed("src2/x.rs", &allow));
    assert!(!path_allowed("other/y.rs", &allow));
}

#[test]
fn prompt_relaxes_test_freeze_for_editable_test_paths() {
    let mk = |paths: Vec<String>| RunOpts {
        spec: "s".into(),
        context_files: vec![],
        apply: false,
        keep_worktree: false,
        run_id: "r".into(),
        builder_model: None,
        editable_paths: paths,
        tier: None,
    };
    let p = build_prompt(&mk(vec!["core/ai.test.ts".into()]), None, None);
    assert!(p.contains("EXCEPTION"));
    let p = build_prompt(&mk(vec!["src/lib.rs".into()]), None, None);
    assert!(!p.contains("EXCEPTION"));
}
```

- [ ] **Step 2: Run to verify they fail to compile** (`path_allowed` undefined)

Run: `cargo test path_allowed_is_boundary_aware`

- [ ] **Step 3: Implement `path_allowed`** (near `is_test_path` in `engine.rs`):

```rust
/// Boundary-aware prefix match, same semantics as scope::check's allowlist:
/// `src` allows `src/x` and `src` itself, but NOT `src2/x`.
fn path_allowed(path: &str, allow: &[String]) -> bool {
    allow.iter().any(|p| {
        let p = p.trim_end_matches('/');
        path == p || path.starts_with(&format!("{p}/"))
    })
}
```

- [ ] **Step 4: Change `reset_test_files` to skip editable paths and return what it reset:**

```rust
fn reset_test_files(workdir: &Path, base_sha: &str, editable_paths: &[String]) -> Vec<String> {
    let mut test_files: Vec<String> = Vec::new();

    // Find tracked test files that differ from base_sha (the model modified them)
    if let Ok(out) = std::process::Command::new("git")
        .args(["diff", "--name-only", base_sha])
        .current_dir(workdir)
        .output()
    {
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            // Test files the caller explicitly allow-listed are the task's
            // deliverable, not a frozen contract — leave them alone.
            if is_test_path(line) && !path_allowed(line, editable_paths) {
                test_files.push(line.to_string());
            }
        }
    }

    if test_files.is_empty() {
        return test_files;
    }

    eprintln!(
        "bob: resetting {} test file(s) the model modified: {}",
        test_files.len(),
        test_files.join(", ")
    );

    // Restore each test file to its base_sha state
    for f in &test_files {
        let _ = std::process::Command::new("git")
            .args(["checkout", base_sha, "--"])
            .arg(f)
            .current_dir(workdir)
            .status();
    }
    test_files
}
```

- [ ] **Step 5: Wire the call site and RunResult.** In `run()`:
  - Above the loop add `let mut reset_files: std::collections::BTreeSet<String> = Default::default();`
  - Replace `reset_test_files(ws.path(), ws.base_sha());` with:

```rust
        for f in reset_test_files(ws.path(), ws.base_sha(), &opts.editable_paths) {
            reset_files.insert(f);
        }
```

  - Add to the `RunResult` struct: `pub reset_test_files: Vec<String>,` and to its construction at the end of `run()`: `reset_test_files: reset_files.into_iter().collect(),`
  - In `src/report.rs` `to_json`, add after `"changed_files"`: `"reset_test_files": &res.reset_test_files,`
  - Fix every `RunResult { ... }` literal in tests (`src/report.rs`, `src/engine.rs`) by adding `reset_test_files: vec![],`.

- [ ] **Step 6: Relax the prompt freeze for editable test paths.** In `build_prompt`, after the `## EDITABLE PATHS` block (inside the `if !opts.editable_paths.is_empty()` branch), add:

```rust
        if opts.editable_paths.iter().any(|p| is_test_path(p)) {
            p.push_str(
                "\nEXCEPTION: test files listed under EDITABLE PATHS are part of this task's \
                 deliverable — you MAY modify them. All other test files remain frozen.\n",
            );
        }
```

- [ ] **Step 7: Run the full suite**

Run: `cargo test` — expected: all pass.

- [ ] **Step 8: Commit**

```bash
git add src/engine.rs src/report.rs && git commit -m "fix(engine): editable_paths exempt test-file reset; report resets in RunResult"
```

---

### Task 4: Replay-verify the diff before reporting converged

**Files:**
- Modify: `src/worktree.rs` (new free fn + `Workspace` method)
- Modify: `src/engine.rs` (`StopReason`, `result_next_action`, the `LoopAction::Apply` arm ~line 1121-1145)
- Modify: `src/config.rs` (`VerifyCfg.replay`, default true)
- Test: `src/worktree.rs` inline test with a temp git repo

**Interfaces:**
- Produces: `pub fn replay_verify_at(repo: &Path, base_sha: &str, run_id: &str, diff: &str, cmds: &[String]) -> anyhow::Result<crate::verify::VerifyResult>` in `worktree.rs`.
- Produces: `impl Workspace { pub fn replay_verify(&self, run_id: &str, diff: &str, cmds: &[String]) -> anyhow::Result<crate::verify::VerifyResult> }` delegating to it.
- Produces: new `StopReason::ReplayVerifyFailed` (Debug string `ReplayVerifyFailed` — additive, hector-safe).
- Produces: `VerifyCfg { cmds, replay: bool }` with `replay` defaulting to `true` (yaml knob `verify.replay: false` to opt out).
- Task 7 reuses `replay_verify_at` for the `bob replay`/`bob apply` verbs.

**Why:** The trust boundary. A converged run must prove that `final_diff` alone reproduces a passing tree at `base_sha` — independent of state the build worktree accumulated. This converts the pilot's failure mode from "confusing judge fail" into an explicit `ReplayVerifyFailed`.

- [ ] **Step 1: Write the failing test** (in `src/worktree.rs`, add a `#[cfg(test)] mod tests` if none exists):

```rust
#[cfg(test)]
mod tests {
    use super::*;

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
```

- [ ] **Step 2: Run to verify it fails to compile** (`replay_verify_at` undefined)

Run: `cargo test replay_verify_applies_diff_and_runs_gates`

- [ ] **Step 3: Implement in `src/worktree.rs`:**

```rust
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
```

And on `Workspace`:

```rust
    pub fn replay_verify(
        &self,
        run_id: &str,
        diff: &str,
        cmds: &[String],
    ) -> anyhow::Result<crate::verify::VerifyResult> {
        replay_verify_at(&self.repo, &self.base_sha, run_id, diff, cmds)
    }
```

- [ ] **Step 4: Config knob.** In `src/config.rs` change `VerifyCfg`:

```rust
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct VerifyCfg {
    #[serde(default)]
    pub cmds: Vec<String>,
    /// Re-apply the final diff to a fresh worktree at base and re-run the
    /// gates before reporting converged. The cost is one extra verify run on
    /// success; the payoff is a diff that is trustworthy for unattended apply.
    #[serde(default = "default_replay")]
    pub replay: bool,
}
fn default_replay() -> bool {
    true
}
```

Note: `Default` derive would set `replay: false` — replace the derive with a manual impl:

```rust
impl Default for VerifyCfg {
    fn default() -> Self {
        Self { cmds: vec![], replay: default_replay() }
    }
}
```

(remove `Default` from the `#[derive(...)]` list).

- [ ] **Step 5: Engine wiring.** Add `ReplayVerifyFailed` to `StopReason`. In `result_next_action`, map it to `NextAction::HumanDecisionRequired` (read the existing match on stop reasons and add the arm in the same style — a replay failure means bob's own capture/replay machinery disagrees with the worktree, which no retry-with-critique can fix). In the `LoopAction::Apply` arm in `run()`, replace the `else` block (currently `status = RunStatus::Converged; if opts.apply { ... }`):

```rust
                } else {
                    let replay_ok = if cfg.verify.replay && !cfg.verify.cmds.is_empty() {
                        match ws.replay_verify(&opts.run_id, &final_diff, &cfg.verify.cmds) {
                            Ok(vr) if vr.passed => true,
                            Ok(vr) => {
                                eprintln!(
                                    "bob: replay-verify FAILED — the final diff does not reproduce a passing tree at base ({})",
                                    vr.cmd.as_deref().unwrap_or("gate")
                                );
                                false
                            }
                            Err(e) => {
                                eprintln!("bob: replay-verify error: {e}");
                                false
                            }
                        }
                    } else {
                        true
                    };
                    if !replay_ok {
                        status = RunStatus::NotConverged;
                        stop_reason = Some(StopReason::ReplayVerifyFailed);
                    } else {
                        status = RunStatus::Converged;
                        if opts.apply {
                            ws.commit_candidate(&format!(
                                "bob: {}",
                                opts.spec.lines().next().unwrap_or("change")
                            ))?;
                            match ws.apply_to_main()? {
                                ApplyOutcome::Applied => applied = true,
                                ApplyOutcome::BaseMoved => {
                                    eprintln!("base moved since run started — not applying; candidate diff returned");
                                }
                            }
                        }
                    }
                }
                break;
```

(Keep the existing secret-scan branch above it untouched.)

- [ ] **Step 6: Run the full suite**

Run: `cargo test` — expected: all pass.

- [ ] **Step 7: Commit**

```bash
git add src/worktree.rs src/engine.rs src/config.rs && git commit -m "feat(engine): replay-verify final diff at base before reporting converged"
```

---

### Task 5: Judge rubric derived from the spec

**Files:**
- Modify: `src/engine.rs` (`build_judge_spec`, ~line 371)
- Test: `src/engine.rs` inline tests

**Interfaces:**
- Consumes: nothing new. Produces: `build_judge_spec` output now ends with a `## JUDGING RUBRIC` section.

**Why:** The judge already receives the full spec; it just isn't told to grade against its acceptance criteria one by one. This makes fail critiques non-empty by construction (belt to Task 1's suspenders).

- [ ] **Step 1: Write the failing test:**

```rust
#[test]
fn judge_spec_carries_rubric_instruction() {
    let s = build_judge_spec("do the thing", None, &[], std::path::Path::new("."));
    assert!(s.contains("## JUDGING RUBRIC"));
    assert!(s.contains("EACH criterion"));
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test judge_spec_carries_rubric_instruction` — expected FAIL.

- [ ] **Step 3: Implement.** At the end of `build_judge_spec`, before `out`:

```rust
    out.push_str(
        "\n\n## JUDGING RUBRIC\n\
         Extract the concrete acceptance criteria from the TASK/SPEC above \
         (explicit bullets, 'must'/'should' clauses, numeric limits, named commands). \
         Evaluate the diff against EACH criterion. List every violated criterion as a \
         disagreement, quoting the criterion text. If all criteria hold, state that \
         explicitly. Never return a fail verdict without naming at least one violated \
         criterion.",
    );
```

- [ ] **Step 4: Run the full suite**

Run: `cargo test` — expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add src/engine.rs && git commit -m "feat(judge): grade against per-criterion rubric derived from the spec"
```

---

### Task 6: Context budget — soft/hard ceilings and token telemetry

**Files:**
- Modify: `src/config.rs` (new `ContextCfg` on `Config`)
- Modify: `src/engine.rs` (replace `warn_on_context_budget` internals with a gate; add `RunResult` fields; record per-iteration prompt tokens)
- Modify: `src/report.rs` (`to_json` + test constructors)
- Test: `src/engine.rs` + `src/config.rs` inline tests

**Interfaces:**
- Produces: `Config.context: ContextCfg { soft_tokens: u64 = 16_000, hard_tokens: u64 = 32_000 }` (yaml: `context: {soft_tokens: ..., hard_tokens: ...}`).
- Produces: `fn context_est_tokens(opts: &RunOpts) -> u64` and `fn enforce_context_budget(opts: &RunOpts, cfg: &ContextCfg) -> anyhow::Result<u64>` in `engine.rs`.
- Produces: `RunResult` fields `pub context_est_tokens: u64` and `pub prompt_est_tokens: Vec<u64>`; JSON keys `"context_est_tokens"`, `"prompt_est_tokens"` (additive).

**Why:** The LAN's local models degrade past ~16k input. Today bob warns at a hard-coded 32k and never refuses; the caller learns nothing about actual per-iteration prompt size.

- [ ] **Step 1: Write the failing tests** (in `src/engine.rs`):

```rust
#[test]
fn context_budget_gate_refuses_over_hard_ceiling() {
    let tmp = std::env::temp_dir().join(format!("bob-ctx-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let big = tmp.join("big.txt");
    std::fs::write(&big, "x".repeat(200_000)).unwrap(); // ~50k est tokens
    let opts = RunOpts {
        spec: "s".into(),
        context_files: vec![big.clone()],
        apply: false,
        keep_worktree: false,
        run_id: "r".into(),
        builder_model: None,
        editable_paths: vec![],
        tier: None,
    };
    let cfg = crate::config::ContextCfg::default(); // soft 16k / hard 32k
    let err = enforce_context_budget(&opts, &cfg).unwrap_err().to_string();
    assert!(err.contains("hard ceiling"), "{err}");
    assert!(err.contains("context.hard_tokens"), "remediation missing: {err}");
    // under the ceiling passes and returns the estimate
    std::fs::write(&big, "x".repeat(4_000)).unwrap();
    let est = enforce_context_budget(&opts, &cfg).unwrap();
    assert!(est >= 1_000 / 4 && est < 16_000, "{est}");
    let _ = std::fs::remove_dir_all(&tmp);
}
```

- [ ] **Step 2: Run to verify it fails to compile** (`ContextCfg`/`enforce_context_budget` undefined)

- [ ] **Step 3: Config.** In `src/config.rs`, next to `LoopCfg`:

```rust
/// Ceilings for the estimated context handed to the builder (bytes/4 ≈ tokens).
/// Local models on this network degrade past ~16k input and choke well before
/// their nominal window — soft warns, hard refuses the run up front.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ContextCfg {
    #[serde(default = "default_soft_tokens")]
    pub soft_tokens: u64,
    #[serde(default = "default_hard_tokens")]
    pub hard_tokens: u64,
}
impl Default for ContextCfg {
    fn default() -> Self {
        Self { soft_tokens: default_soft_tokens(), hard_tokens: default_hard_tokens() }
    }
}
fn default_soft_tokens() -> u64 {
    16_000
}
fn default_hard_tokens() -> u64 {
    32_000
}
```

Add to `Config` (mirror how other sections are declared, with `#[serde(default)]`): `pub context: ContextCfg,`. If `Config` derives `Default` or has a manual impl, extend it accordingly.

- [ ] **Step 4: Engine.** Refactor `warn_on_context_budget` into two functions (keep the existing per-file eprintln reporting inside the estimator):

```rust
fn context_est_tokens(opts: &RunOpts) -> u64 {
    // (move the existing file-collection + total/4 logic from
    // warn_on_context_budget here; keep the informational eprintln; return est_tokens,
    // 0 when no files)
}

/// Pre-flight gate: refuse runs whose context estimate exceeds the hard
/// ceiling (the builder would stall long before producing anything useful),
/// warn past the soft one. Returns the estimate for RunResult telemetry.
fn enforce_context_budget(
    opts: &RunOpts,
    cfg: &crate::config::ContextCfg,
) -> anyhow::Result<u64> {
    let est = context_est_tokens(opts);
    if est > cfg.hard_tokens {
        anyhow::bail!(
            "context ~{}k tokens exceeds hard ceiling {}k — trim files/editable_paths, \
             pass excerpts instead of whole files, or raise context.hard_tokens in bob.yaml",
            est / 1000,
            cfg.hard_tokens / 1000
        );
    }
    if est > cfg.soft_tokens {
        eprintln!(
            "bob: ⚠️  context ~{}k tokens exceeds soft ceiling {}k — local builders degrade past this; consider trimming",
            est / 1000,
            cfg.soft_tokens / 1000
        );
    }
    Ok(est)
}
```

Replace the `warn_on_context_budget(&opts);` call site (engine.rs ~line 779) with `let context_est = enforce_context_budget(&opts, &cfg.context)?;` and thread `context_est` through to where `run()`'s `RunResult` is built (if the call site is in `run_opencode_with_fallbacks` rather than `run()`, move the gate to the top of `run()` instead — one gate, before `Workspace::create`).

- [ ] **Step 5: Telemetry.** Add `RunResult` fields `pub context_est_tokens: u64, pub prompt_est_tokens: Vec<u64>`. In the loop, right after `let prompt = build_prompt(...)`, push `(prompt.len() as u64) / 4` onto a `prompt_est_tokens` vec; set both fields in the result. Add both keys to `report::to_json`. Fix all `RunResult` literals in tests (`context_est_tokens: 0, prompt_est_tokens: vec![]`).

- [ ] **Step 6: Run the full suite**

Run: `cargo test` — expected: all pass.

- [ ] **Step 7: Commit**

```bash
git add src/config.rs src/engine.rs src/report.rs && git commit -m "feat(engine): context soft/hard ceilings + token telemetry in RunResult"
```

---

### Task 7: run.json, events.jsonl, and `bob replay` / `bob apply` verbs

**Files:**
- Modify: `src/report.rs` (`append_event`, and persist `run.json`)
- Modify: `src/engine.rs` (emit events; write `run.json`; new `RunResult` field `verify_cmds`)
- Modify: `src/cli.rs`, `src/main.rs` (two new subcommands)
- Test: `src/report.rs` inline test

**Interfaces:**
- Produces: `pub fn append_event(dir: &Path, run_id: &str, event: serde_json::Value)` in `report.rs` (infallible — best-effort logging).
- Produces: `<artifacts.dir>/<run_id>/run.json` = the full RunResult JSON (written even for failed runs).
- Produces: `RunResult.verify_cmds: Vec<String>` + JSON key `"verify_cmds"` (needed so replay uses the run's gates, not whatever bob.yaml says today).
- Consumes: `worktree::replay_verify_at` from Task 4.
- CLI: `bob replay <run_id>` (replay-verify only, exit 1 on fail) and `bob apply <run_id>` (replay-verify then `git apply` to the working tree, refusing if HEAD moved off `base_sha`).

**Why:** `stderr_tail` was a wall of ANSI in the pilot; triage needs a structured timeline. And landing a preserved run by hand (10 human-minutes in the pilot) should be one verb.

- [ ] **Step 1: Write the failing test** (in `src/report.rs`):

```rust
#[test]
fn append_event_writes_jsonl() {
    let tmp = std::env::temp_dir().join(format!("bob-ev-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    append_event(&tmp, "r1", serde_json::json!({"event": "verify", "passed": true}));
    append_event(&tmp, "r1", serde_json::json!({"event": "judge", "verdict": "pass"}));
    let text = std::fs::read_to_string(tmp.join("r1/events.jsonl")).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2);
    let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(first["event"], "verify");
    assert!(first["ts"].as_u64().unwrap() > 0);
    let _ = std::fs::remove_dir_all(&tmp);
}
```

- [ ] **Step 2: Run to verify it fails to compile**, then implement in `report.rs`:

```rust
/// Best-effort structured event log: one JSON object per line in
/// <dir>/<run_id>/events.jsonl, stamped with unix seconds. Never fails the run.
pub fn append_event(dir: &Path, run_id: &str, mut event: serde_json::Value) {
    use std::io::Write;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if let Some(o) = event.as_object_mut() {
        o.insert("ts".into(), serde_json::json!(ts));
    }
    let d = dir.join(run_id);
    if std::fs::create_dir_all(&d).is_err() {
        return;
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(d.join("events.jsonl"))
    {
        let _ = writeln!(f, "{event}");
    }
}
```

- [ ] **Step 3: Emit events from `run()`** (use `let art = std::path::Path::new(&cfg.artifacts.dir);` — `append_event(art, &opts.run_id, json!({...}))`):
  - run start (after workspace creation): `{"event":"run_start","base_sha":base_sha,"model":opts.builder_model}`
  - after each builder call: `{"event":"builder_done","iter":state.index,"failure_kind":builder_snapshot.failure_kind}`
  - when `reset_test_files` returned non-empty: `{"event":"test_files_reset","files":[...]}`
  - after verify: `{"event":"verify","iter":state.index,"passed":vr.passed}`
  - after judge: `{"event":"judge","iter":state.index,"verdict":verdict_name(o.verdict),"critique_empty":o.critique.trim().is_empty()}`
  - after replay-verify: `{"event":"replay_verify","passed":replay_ok}`
  - run end (just before constructing the result is too early — do it right after): `{"event":"run_end","status":<status string>,"stop_reason":<debug string or "">}`

- [ ] **Step 4: Persist run.json + verify_cmds.** Add `pub verify_cmds: Vec<String>` to `RunResult` (populate from `cfg.verify.cmds.clone()`; add `"verify_cmds"` to `to_json`; fix test literals). At the end of `run()` after building `result`:

```rust
    let _ = std::fs::create_dir_all(std::path::Path::new(&result.artifact_dir));
    let _ = std::fs::write(
        std::path::Path::new(&result.artifact_dir).join("run.json"),
        crate::report::to_json(&result),
    );
```

- [ ] **Step 5: CLI verbs.** In `src/cli.rs` add to `Command`:

```rust
    /// Replay-verify a past run: apply its final_diff to a fresh tree at its
    /// base_sha and re-run its verify gates. Read-only for your working tree.
    Replay {
        /// Run id (directory name under the artifacts dir).
        run_id: String,
    },
    /// Replay-verify a past run, then apply its final_diff to the working tree.
    /// Refuses if HEAD has moved off the run's base_sha.
    Apply {
        run_id: String,
    },
```

In `src/main.rs`, following the style of the existing handlers, add a helper and the two arms:

```rust
fn load_run_json(artifacts_dir: &str, run_id: &str) -> anyhow::Result<serde_json::Value> {
    let path = std::path::Path::new(artifacts_dir).join(run_id).join("run.json");
    let text = std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("no run.json for {run_id} at {}: {e}", path.display()))?;
    Ok(serde_json::from_str(&text)?)
}

fn replay_run(cfg: &bob::config::Config, run_id: &str) -> anyhow::Result<(serde_json::Value, bool)> {
    let run = load_run_json(&cfg.artifacts.dir, run_id)?;
    let base_sha = run["base_sha"].as_str().unwrap_or_default().to_string();
    let diff = run["final_diff"].as_str().unwrap_or_default().to_string();
    if diff.trim().is_empty() {
        anyhow::bail!("run {run_id} has an empty final_diff — nothing to replay");
    }
    let cmds: Vec<String> = run["verify_cmds"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let cmds = if cmds.is_empty() { cfg.verify.cmds.clone() } else { cmds };
    let repo = std::env::current_dir()?;
    let vr = bob::worktree::replay_verify_at(&repo, &base_sha, run_id, &diff, &cmds)?;
    println!(
        "bob: replay-verify {} for run {run_id} ({} gate(s))",
        if vr.passed { "PASSED" } else { "FAILED" },
        cmds.len()
    );
    Ok((run, vr.passed))
}
```

(Adjust the module paths — `bob::` vs `crate::` — to match how `main.rs` already refers to the library modules.)

Arms:

```rust
        Command::Replay { run_id } => {
            let (_, passed) = replay_run(&cfg, &run_id)?;
            if !passed {
                std::process::exit(1);
            }
        }
        Command::Apply { run_id } => {
            let (run, passed) = replay_run(&cfg, &run_id)?;
            if !passed {
                anyhow::bail!("replay-verify failed — not applying");
            }
            let base_sha = run["base_sha"].as_str().unwrap_or_default();
            let cwd = std::env::current_dir()?;
            let head = std::process::Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(&cwd)
                .output()?;
            let head = String::from_utf8_lossy(&head.stdout).trim().to_string();
            if head != base_sha {
                anyhow::bail!(
                    "HEAD ({head}) has moved off the run's base_sha ({base_sha}) — rebase/re-run instead of applying"
                );
            }
            let patch = std::path::Path::new(&cfg.artifacts.dir).join(&run_id).join("apply.patch");
            std::fs::write(&patch, run["final_diff"].as_str().unwrap_or_default())?;
            let st = std::process::Command::new("git")
                .args(["apply", "--whitespace=nowarn"])
                .arg(&patch)
                .current_dir(&cwd)
                .status()?;
            if !st.success() {
                anyhow::bail!("git apply failed");
            }
            println!("bob: applied run {run_id} to the working tree (unstaged)");
        }
```

Note: `worktree::replay_verify_at` and the `worktree` module must be reachable from `main.rs` — make the module `pub` in the crate root if it isn't already (check how `main.rs` reaches `engine`/`report` and follow suit).

- [ ] **Step 6: Run the full suite + smoke the CLI**

Run: `cargo test` — all pass. Then `cargo run -- replay no-such-run` — expected: clean error mentioning `run.json`.

- [ ] **Step 7: Commit**

```bash
git add src/report.rs src/engine.rs src/cli.rs src/main.rs && git commit -m "feat: events.jsonl + run.json artifacts; bob replay/apply verbs"
```

---

### Task 8: Campaign-level integration verify

**Files:**
- Modify: `src/campaign.rs` (`Campaign`, `CampaignReport`, `run`)
- Test: `src/campaign.rs` inline tests

**Interfaces:**
- Produces: `Campaign.verify_cmds: Vec<String>` (campaign-level, yaml key `verify_cmds` at the top level of the campaign file).
- Produces: `CampaignReport.campaign_verify: Option<CampaignVerify>` where

```rust
#[derive(Debug, Serialize)]
pub struct CampaignVerify {
    pub passed: bool,
    pub cmd: Option<String>,
    pub output_tail: String,
}
```

- Produces: report `status` value `"integration_failed"` when all slices converge but the campaign gate fails.

**Why:** Per-slice verification can't see cross-slice interference; the doc's ask is one integration gate after all slices land. Only meaningful for `auto_apply`/`auto_commit` campaigns (changes are in the main tree); harmless otherwise.

- [ ] **Step 1: Write the failing test** (model on the existing yaml-parsing tests around `src/campaign.rs:390`):

```rust
#[test]
fn campaign_level_verify_cmds_parse() {
    let y = "name: c\nverify_cmds: [\"npm run test:all\"]\nslices:\n  - task: t\n";
    let c: Campaign = serde_yaml::from_str(y).unwrap();
    assert_eq!(c.verify_cmds, vec!["npm run test:all"]);
}
```

- [ ] **Step 2: Run to verify it fails to compile** (no such field), then add `#[serde(default)] pub verify_cmds: Vec<String>,` to `Campaign` and the `campaign_verify` field + `CampaignVerify` struct to the report types. Fix any struct literals in tests (`verify_cmds: vec![]`, `campaign_verify: None`).

- [ ] **Step 3: Wire into `run()`.** After the slice loop, before building the report:

```rust
    let mut campaign_verify = None;
    let mut status = if completed { "completed" } else { "stopped" }.to_string();
    if completed && !campaign.verify_cmds.is_empty() {
        let cwd = std::env::current_dir()?;
        let vr = crate::verify::run_gates(&campaign.verify_cmds, &cwd);
        if !vr.passed {
            status = "integration_failed".into();
        }
        campaign_verify = Some(CampaignVerify {
            passed: vr.passed,
            cmd: vr.cmd.clone(),
            output_tail: crate::builder::tail(&vr.output, 4000),
        });
    }
    let report = CampaignReport {
        name,
        status,
        result_path: result_path_str,
        slices: reports,
        campaign_verify,
    };
```

(Check `verify::VerifyResult`'s exact field names in `src/verify.rs` and match them.)

- [ ] **Step 4: Run the full suite**

Run: `cargo test` — expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add src/campaign.rs && git commit -m "feat(campaign): campaign-level integration verify after all slices land"
```

---

### Task 9: Polish — tool-param prompt rule, version bump, docs

**Files:**
- Modify: `src/engine.rs` (`build_prompt` — one rule line)
- Modify: `Cargo.toml` (version `0.2.13` → `0.3.0`)
- Modify: `README.md` (document: replay-verify + `verify.replay`, `context` ceilings, `bob replay`/`bob apply`, campaign `verify_cmds`, editable test paths, worktree retention, `events.jsonl`/`run.json`)

**Why:** Behavior changed in user-visible ways (hard context ceiling, replay default-on, worktrees now kept on failure); the version must say so. The tool-param line is the cheap rung of the doc's item 8 (the pilot's Qwen3 builder emitted a `Write` with no `content`).

- [ ] **Step 1: Add the rule line** in `build_prompt`, after the existing `- Implement to make the test/gate pass` rule:

```rust
         - Every tool call must include ALL required parameters (e.g. a file write needs both the path AND the full content). Never emit a partial tool call.\n\
```

(Keep it inside the same `format!` string block, matching the existing `\n\` continuation style.)

- [ ] **Step 2: Bump the version** in `Cargo.toml`: `version = "0.3.0"`. Run `cargo build` so `Cargo.lock` updates, and commit the lockfile change too.

- [ ] **Step 3: Update README.md.** Add/extend sections covering each new knob and verb listed above — short, one paragraph or bullet each, in the README's existing voice. Include one example: `bob replay <run_id>` after a failed unattended run.

- [ ] **Step 4: Full gate**

Run: `cargo test && cargo build --release`
Expected: all tests pass, release build clean.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "chore: bump to 0.3.0; document trust-layer features; tool-param prompt rule"
```

---

## Deliberately out of scope

- **maple context bundles (doc item 5c)** — new cross-tool integration; separate design effort.
- **Threshold-based toolshim auto-routing (doc item 8)** — goose toolshim already exists (commit 39272d4); the prompt rule is the cheap rung. Revisit if schema-error rates show up in `events.jsonl`.
- **Refuse-at-dispatch when a spec "requires" test edits** — undecidable from prose; the editable-paths exemption plus the structured `reset_test_files` report covers the failure mode observably.
- **hector-side items** — hector will do its own (per user).
- **Pre-existing gap, noticed but not touched:** `reset_test_files` cannot actually revert *frozen* (post-base committed) test files — `git checkout <base_sha> -- f` fails silently for files that don't exist at base. Exists before this plan; unchanged by it.
