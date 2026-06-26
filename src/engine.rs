use std::path::PathBuf;
use std::time::{Duration, Instant};
use crate::config::Config;
use crate::builder::Builder;
use crate::judge::{Judge, Verdict};
use crate::verify::run_gates;
use crate::worktree::{Workspace, ApplyOutcome};
use crate::scope;

#[derive(Debug)]
pub enum LoopAction {
    Apply,
    Continue { critique: String },
    Stop { reason: StopReason },
}

#[derive(Debug, PartialEq, Eq)]
pub enum StopReason {
    MaxIterations, Walltime, EmptyDiffAfterCritique,
    RepeatedVerifyFailure, RepeatedUncertain, ScopeExceeded, SecretScanBlocked,
}

/// Mutable per-loop history the decision function reads.
pub struct LoopState {
    pub index: u32,
    pub max_iterations: u32,
    pub had_critique: bool,
    pub last_verify_fail: Option<String>,
    pub uncertain_streak: u32,
    pub walltime_exceeded: bool,
}

/// What happened in one build→(verify)→(judge) pass.
pub enum StepOutcome {
    EmptyDiff,
    ScopeExceeded { detail: String },
    VerifyFailed { output: String },
    Judged { verdict: Verdict, critique: String },
}
impl StepOutcome {
    pub fn empty_diff() -> Self { StepOutcome::EmptyDiff }
    pub fn scope_exceeded(d: &str) -> Self { StepOutcome::ScopeExceeded { detail: d.into() } }
    pub fn verify_failed(o: &str) -> Self { StepOutcome::VerifyFailed { output: o.into() } }
    pub fn judged(_passed_verify: bool, v: Verdict, c: &str) -> Self {
        StepOutcome::Judged { verdict: v, critique: c.into() }
    }
}

/// Pure decision: given history + this step's outcome, what next?
pub fn next_action(state: &LoopState, step: &StepOutcome) -> LoopAction {
    if state.walltime_exceeded {
        return LoopAction::Stop { reason: StopReason::Walltime };
    }
    let at_last = state.index + 1 >= state.max_iterations;
    match step {
        StepOutcome::EmptyDiff => {
            if state.had_critique {
                LoopAction::Stop { reason: StopReason::EmptyDiffAfterCritique }
            } else if at_last {
                LoopAction::Stop { reason: StopReason::MaxIterations }
            } else {
                LoopAction::Continue { critique: "no changes were produced; make the edits the task requires".into() }
            }
        }
        StepOutcome::ScopeExceeded { .. } => LoopAction::Stop { reason: StopReason::ScopeExceeded },
        StepOutcome::VerifyFailed { output } => {
            if state.last_verify_fail.as_deref() == Some(output.as_str()) {
                LoopAction::Stop { reason: StopReason::RepeatedVerifyFailure }
            } else if at_last {
                LoopAction::Stop { reason: StopReason::MaxIterations }
            } else {
                LoopAction::Continue { critique: format!("verify failed; fix this:\n{output}") }
            }
        }
        StepOutcome::Judged { verdict, critique } => match verdict {
            Verdict::Pass => LoopAction::Apply,
            Verdict::Uncertain if state.uncertain_streak >= 1 =>
                LoopAction::Stop { reason: StopReason::RepeatedUncertain },
            _ if at_last => LoopAction::Stop { reason: StopReason::MaxIterations },
            _ => LoopAction::Continue { critique: critique.clone() },
        },
    }
}

pub struct RunOpts {
    pub spec: String,
    pub context_files: Vec<PathBuf>,
    pub apply: bool,
    pub keep: bool,
    pub run_id: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RunStatus { Converged, NotConverged, Error }

pub struct RunResult {
    pub status: RunStatus,
    pub base_sha: String,
    pub iterations: u32,
    pub final_diff: String,
    pub applied: bool,
    pub stop_reason: Option<StopReason>,
}

fn build_prompt(opts: &RunOpts, critique: Option<&str>) -> String {
    let mut p = format!("## TASK / SPEC\n{}\n", opts.spec);
    if !opts.context_files.is_empty() {
        p.push_str("\n## CONTEXT FILES\n");
        for f in &opts.context_files { p.push_str(&format!("- {}\n", f.display())); }
    }
    if let Some(c) = critique {
        p.push_str(&format!("\n## PREVIOUS ATTEMPT WAS REJECTED — FIX THIS\n{c}\n"));
    }
    p
}

#[allow(unused_assignments)] // final_diff init is dead; loop always overwrites before any break
pub async fn run(
    cfg: &Config,
    opts: RunOpts,
    builder: &impl Builder,
    judge: &impl Judge,
) -> anyhow::Result<RunResult> {
    if cfg.verify.cmds.is_empty() {
        eprintln!("warning: no verify gates configured — abe is the sole gate");
    }
    // Secret-scan inputs before anything enters a prompt.
    let spec_hits = crate::safety::scan(&opts.spec);
    if !spec_hits.is_empty() {
        anyhow::bail!("secret-scan flagged the spec/task body: {:?}", spec_hits);
    }
    for f in &opts.context_files {
        if crate::safety::risky_filename(&f.to_string_lossy()) {
            anyhow::bail!("refusing: context file looks sensitive: {}", f.display());
        }
        if let Ok(text) = std::fs::read_to_string(f) {
            let hits = crate::safety::scan(&text);
            if !hits.is_empty() {
                anyhow::bail!("secret-scan flagged {}: {:?}", f.display(), hits);
            }
        }
    }
    let ws = Workspace::create(&opts.run_id)?;
    let base_sha = ws.base_sha().to_string();
    let deadline = Instant::now() + Duration::from_secs(cfg.loop_cfg.max_walltime_secs);

    let mut state = LoopState {
        index: 0, max_iterations: cfg.loop_cfg.max_iterations, had_critique: false,
        last_verify_fail: None, uncertain_streak: 0, walltime_exceeded: false,
    };
    let mut critique: Option<String> = None;
    let mut final_diff = String::new();
    let mut applied = false;
    let mut stop_reason = None;
    let mut status = RunStatus::NotConverged;

    loop {
        state.walltime_exceeded = Instant::now() >= deadline;
        let prompt = build_prompt(&opts, critique.as_deref());

        // BUILD
        builder.build(&prompt, ws.path()).await?;
        let diff = ws.capture_diff()?;
        final_diff = diff.clone();

        // STEP OUTCOME
        let step = if diff.trim().is_empty() {
            StepOutcome::EmptyDiff
        } else {
            let sr = scope::check(&diff, &cfg.scope);
            if !sr.within {
                StepOutcome::scope_exceeded(&sr.detail)
            } else {
                let vr = run_gates(&cfg.verify.cmds, ws.path());
                if !vr.passed {
                    StepOutcome::VerifyFailed { output: vr.output }
                } else {
                    // Verify gate is the authority: a passing objective gate means
                    // converged. abe runs as a NON-BLOCKING advisory second opinion
                    // (it's adversarial and returns no structured pass/fail), so its
                    // take is surfaced but never gates convergence or fails the run.
                    match judge.judge(&opts.spec, &diff, &vr.output).await {
                        Ok(o) if !o.critique.trim().is_empty() =>
                            eprintln!("abe advisory (non-blocking):\n{}\n", o.critique),
                        Ok(_) => {}
                        Err(e) => eprintln!("abe advisory unavailable (non-blocking): {e}"),
                    }
                    StepOutcome::Judged { verdict: Verdict::Pass, critique: String::new() }
                }
            }
        };

        let verdict_label = match &step {
            StepOutcome::EmptyDiff => "empty-diff".to_string(),
            StepOutcome::ScopeExceeded { detail } => format!("scope-exceeded: {detail}"),
            StepOutcome::VerifyFailed { .. } => "verify-failed".to_string(),
            StepOutcome::Judged { verdict, .. } => format!("{verdict:?}"),
        };
        let _ = crate::report::write_artifacts(
            std::path::Path::new(&cfg.artifacts.dir), &opts.run_id, state.index,
            &prompt, &final_diff, &verdict_label);

        let action = next_action(&state, &step);

        // update streaks AFTER deciding
        if let StepOutcome::Judged { verdict: Verdict::Uncertain, .. } = &step {
            state.uncertain_streak += 1;
        } else if let StepOutcome::Judged { .. } = &step {
            state.uncertain_streak = 0;
        }

        // remember verify failure for repeat detection
        if let StepOutcome::VerifyFailed { output } = &step {
            state.last_verify_fail = Some(output.clone());
        }

        match action {
            LoopAction::Apply => {
                status = RunStatus::Converged;
                let diff_hits = crate::safety::scan(&final_diff);
                if !diff_hits.is_empty() {
                    eprintln!("secret-scan flagged the candidate diff; NOT applying: {diff_hits:?}");
                    stop_reason = Some(StopReason::SecretScanBlocked);
                } else if opts.apply {
                    ws.commit_candidate(&format!("bob: {}", opts.spec.lines().next().unwrap_or("change")))?;
                    match ws.apply_to_main()? {
                        ApplyOutcome::Applied => applied = true,
                        ApplyOutcome::BaseMoved => {
                            eprintln!("base moved since run started — not applying; candidate diff returned");
                        }
                    }
                }
                break;
            }
            LoopAction::Continue { critique: c } => {
                critique = Some(c);
                state.had_critique = true;
                state.index += 1;
            }
            LoopAction::Stop { reason } => { stop_reason = Some(reason); break; }
        }
    }

    let result = RunResult {
        status, base_sha, iterations: state.index + 1, final_diff, applied, stop_reason,
    };
    if opts.keep || result.status != RunStatus::Converged {
        eprintln!("worktree preserved at {}", ws.path().display());
    } else {
        ws.cleanup()?;
    }
    Ok(result)
}

#[cfg(test)]
mod decision_tests {
    use super::*;

    fn state(index: u32, max: u32) -> LoopState {
        LoopState { index, max_iterations: max, had_critique: index > 0,
                    last_verify_fail: None, uncertain_streak: 0, walltime_exceeded: false }
    }

    #[test]
    fn pass_verdict_applies() {
        let s = state(0, 3);
        let step = StepOutcome::judged(true, Verdict::Pass, "ok");
        assert!(matches!(next_action(&s, &step), LoopAction::Apply));
    }
    #[test]
    fn verify_fail_continues_with_verify_output() {
        let s = state(0, 3);
        let step = StepOutcome::verify_failed("cargo test failed: X");
        match next_action(&s, &step) {
            LoopAction::Continue { critique } => assert!(critique.contains("cargo test failed")),
            other => panic!("expected Continue, got {other:?}"),
        }
    }
    #[test]
    fn repeated_identical_verify_failure_stops() {
        let mut s = state(1, 3);
        s.last_verify_fail = Some("same error".to_string());
        let step = StepOutcome::verify_failed("same error");
        assert!(matches!(next_action(&s, &step),
            LoopAction::Stop { reason: StopReason::RepeatedVerifyFailure }));
    }
    #[test]
    fn empty_diff_after_critique_stops() {
        let s = state(1, 3); // had_critique == true
        let step = StepOutcome::empty_diff();
        assert!(matches!(next_action(&s, &step),
            LoopAction::Stop { reason: StopReason::EmptyDiffAfterCritique }));
    }
    #[test]
    fn fail_verdict_continues_with_critique() {
        let s = state(0, 3);
        let step = StepOutcome::judged(true, Verdict::Fail, "missing X");
        match next_action(&s, &step) {
            LoopAction::Continue { critique } => assert!(critique.contains("missing X")),
            other => panic!("expected Continue, got {other:?}"),
        }
    }
    #[test]
    fn first_uncertain_continues() {
        let s = state(0, 3); // streak 0
        let step = StepOutcome::judged(true, Verdict::Uncertain, "unsure");
        assert!(matches!(next_action(&s, &step), LoopAction::Continue { .. }));
    }
    #[test]
    fn two_uncertain_in_a_row_stops() {
        let mut s = state(1, 3);
        s.uncertain_streak = 1;
        let step = StepOutcome::judged(true, Verdict::Uncertain, "unsure");
        assert!(matches!(next_action(&s, &step),
            LoopAction::Stop { reason: StopReason::RepeatedUncertain }));
    }
    #[test]
    fn last_iteration_fail_stops_at_max() {
        let s = state(2, 3); // index 2 is the 3rd (0-based); next would be == max
        let step = StepOutcome::judged(true, Verdict::Fail, "still wrong");
        assert!(matches!(next_action(&s, &step),
            LoopAction::Stop { reason: StopReason::MaxIterations }));
    }
    #[test]
    fn scope_exceeded_stops() {
        let s = state(0, 3);
        let step = StepOutcome::scope_exceeded("21 files");
        assert!(matches!(next_action(&s, &step),
            LoopAction::Stop { reason: StopReason::ScopeExceeded }));
    }
}

#[cfg(test)]
mod flow_tests {
    use super::*;
    use crate::judge::JudgeOutcome;
    use std::path::Path;
    use std::cell::Cell;

    // Makes NO change on the first call, then a real change — exercises the
    // empty-diff retry path (the retry trigger under the verify-authority model).
    struct FlakyBuilder { calls: Cell<u32> }
    impl Builder for FlakyBuilder {
        async fn build(&self, _p: &str, workdir: &Path) -> anyhow::Result<()> {
            let n = self.calls.get(); self.calls.set(n + 1);
            if n >= 1 { std::fs::write(workdir.join("out.txt"), "change\n")?; }
            Ok(())
        }
    }
    // Always Uncertain. Under verify-authority this verdict is advisory and must
    // NOT block convergence — the test asserts exactly that.
    struct UncertainJudge;
    impl Judge for UncertainJudge {
        async fn judge(&self, _s: &str, _d: &str, _v: &str) -> anyhow::Result<JudgeOutcome> {
            Ok(JudgeOutcome { verdict: Verdict::Uncertain, critique: "advisory only".into() })
        }
    }

    #[tokio::test]
    async fn empty_diff_retries_then_verify_pass_converges() {
        let _cwd_guard = crate::CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // requires running inside a temp git repo; see worktree::tests helper.
        let tmp = std::env::temp_dir().join(format!("bob-flow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let g = |a: &[&str]| { std::process::Command::new("git").args(a).current_dir(&tmp).output().unwrap(); };
        g(&["init","-q"]); g(&["config","user.email","t@t"]); g(&["config","user.name","t"]);
        std::fs::write(tmp.join("seed.txt"), "x\n").unwrap();
        g(&["add","."]); g(&["commit","-qm","init"]);

        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&tmp).unwrap();

        let cfg = crate::config::Config {
            builder: crate::config::BuilderCfg { cmd: "opencode".into(), timeout_secs: 5, args: vec![], model: None, models: Default::default() },
            judge: crate::config::JudgeCfg { cmd: "abe".into(), mode: crate::config::JudgeMode::Validate, timeout_secs: 600 },
            verify: crate::config::VerifyCfg { cmds: vec!["true".into()] }, // gate that passes
            loop_cfg: crate::config::LoopCfg { max_iterations: 3, max_walltime_secs: 60 },
            scope: Default::default(), apply: false, artifacts: Default::default(),
        };
        let opts = RunOpts { spec: "do the thing".into(), context_files: vec![],
                             apply: false, keep: false, run_id: "flow".into() };
        let res = run(&cfg, opts, &FlakyBuilder { calls: Cell::new(0) }, &UncertainJudge).await.unwrap();

        std::env::set_current_dir(prev).unwrap();
        // iter-0: empty diff -> continue; iter-1: change -> verify gate passes ->
        // converge, despite the judge returning Uncertain (now advisory only).
        assert_eq!(res.status, RunStatus::Converged);
        assert_eq!(res.iterations, 2);
    }
}
