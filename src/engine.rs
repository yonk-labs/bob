use crate::builder::{Builder, BuilderOutcome, Opencode};
use crate::config::{Config, JudgePolicy};
use crate::judge::{Abe, Judge, Verdict};
use crate::scope;
use crate::verify::run_gates;
use crate::worktree::{ApplyOutcome, Workspace};
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Debug)]
pub enum LoopAction {
    Apply,
    Continue { critique: String },
    Stop { reason: StopReason },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    MaxIterations,
    Walltime,
    EmptyDiffAfterCritique,
    RepeatedVerifyFailure,
    RepeatedUncertain,
    ScopeExceeded,
    SecretScanBlocked,
    JudgeRejected,
    JudgeUnavailable,
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
    JudgeUnavailable { detail: String },
    Judged { verdict: Verdict, critique: String },
}
impl StepOutcome {
    pub fn empty_diff() -> Self {
        StepOutcome::EmptyDiff
    }
    pub fn scope_exceeded(d: &str) -> Self {
        StepOutcome::ScopeExceeded { detail: d.into() }
    }
    pub fn verify_failed(o: &str) -> Self {
        StepOutcome::VerifyFailed { output: o.into() }
    }
    pub fn judge_unavailable(d: &str) -> Self {
        StepOutcome::JudgeUnavailable { detail: d.into() }
    }
    pub fn judged(_passed_verify: bool, v: Verdict, c: &str) -> Self {
        StepOutcome::Judged {
            verdict: v,
            critique: c.into(),
        }
    }
}

/// Pure decision: given history + this step's outcome, what next?
pub fn next_action(state: &LoopState, step: &StepOutcome, judge_policy: JudgePolicy) -> LoopAction {
    if state.walltime_exceeded {
        return LoopAction::Stop {
            reason: StopReason::Walltime,
        };
    }
    let at_last = state.index + 1 >= state.max_iterations;
    match step {
        StepOutcome::EmptyDiff => {
            if state.had_critique {
                LoopAction::Stop {
                    reason: StopReason::EmptyDiffAfterCritique,
                }
            } else if at_last {
                LoopAction::Stop {
                    reason: StopReason::MaxIterations,
                }
            } else {
                LoopAction::Continue {
                    critique: "no changes were produced; make the edits the task requires".into(),
                }
            }
        }
        StepOutcome::ScopeExceeded { .. } => LoopAction::Stop {
            reason: StopReason::ScopeExceeded,
        },
        StepOutcome::VerifyFailed { output } => {
            if state.last_verify_fail.as_deref() == Some(output.as_str()) {
                LoopAction::Stop {
                    reason: StopReason::RepeatedVerifyFailure,
                }
            } else if at_last {
                LoopAction::Stop {
                    reason: StopReason::MaxIterations,
                }
            } else {
                LoopAction::Continue {
                    critique: format!("verify failed; fix this:\n{output}"),
                }
            }
        }
        StepOutcome::JudgeUnavailable { .. } => match judge_policy {
            JudgePolicy::Advisory => LoopAction::Apply,
            JudgePolicy::Blocking | JudgePolicy::RetryOnFail => LoopAction::Stop {
                reason: StopReason::JudgeUnavailable,
            },
        },
        StepOutcome::Judged { verdict, critique } => match judge_policy {
            JudgePolicy::Advisory => LoopAction::Apply,
            JudgePolicy::Blocking => match verdict {
                Verdict::Pass => LoopAction::Apply,
                Verdict::Fail | Verdict::Uncertain => LoopAction::Stop {
                    reason: StopReason::JudgeRejected,
                },
            },
            JudgePolicy::RetryOnFail => match verdict {
                Verdict::Pass => LoopAction::Apply,
                Verdict::Uncertain if critique.trim().is_empty() => LoopAction::Apply,
                Verdict::Uncertain if state.uncertain_streak >= 1 => LoopAction::Stop {
                    reason: StopReason::RepeatedUncertain,
                },
                _ if at_last => LoopAction::Stop {
                    reason: StopReason::MaxIterations,
                },
                _ => LoopAction::Continue {
                    critique: critique.clone(),
                },
            },
        },
    }
}

pub struct RunOpts {
    pub spec: String,
    pub context_files: Vec<PathBuf>,
    pub apply: bool,
    pub keep_worktree: bool,
    pub run_id: String,
    pub builder_model: Option<String>,
}

impl Clone for RunOpts {
    fn clone(&self) -> Self {
        Self {
            spec: self.spec.clone(),
            context_files: self.context_files.clone(),
            apply: self.apply,
            keep_worktree: self.keep_worktree,
            run_id: self.run_id.clone(),
            builder_model: self.builder_model.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    Converged,
    NeedsReview,
    NotConverged,
    Error,
}

#[derive(Debug, PartialEq, Eq)]
pub enum NextAction {
    Done,
    ReviewCandidate,
    RetryWithJudgeCritique,
    RetryWithVerifyFailure,
    SplitTask,
    EscalateModel,
    HumanDecisionRequired,
}

impl NextAction {
    pub fn as_str(&self) -> &'static str {
        match self {
            NextAction::Done => "done",
            NextAction::ReviewCandidate => "review_candidate",
            NextAction::RetryWithJudgeCritique => "retry_with_judge_critique",
            NextAction::RetryWithVerifyFailure => "retry_with_verify_failure",
            NextAction::SplitTask => "split_task",
            NextAction::EscalateModel => "escalate_model",
            NextAction::HumanDecisionRequired => "human_decision_required",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ScopeSnapshot {
    pub within: bool,
    pub files: usize,
    pub lines: usize,
    pub detail: String,
}

#[derive(Debug, Clone)]
pub struct VerifySnapshot {
    pub passed: bool,
    pub cmd: Option<String>,
    pub output_tail: String,
}

#[derive(Debug, Clone)]
pub struct JudgeSnapshot {
    pub policy: JudgePolicy,
    pub verdict: String,
    pub critique: String,
}

#[derive(Debug, Clone)]
pub struct BuilderSnapshot {
    pub model: Option<String>,
    pub stdout_tail: String,
    pub stderr_tail: String,
    pub failure_kind: String,
    pub fallbacks_tried: Vec<String>,
}

pub struct RunResult {
    pub status: RunStatus,
    pub next_action: NextAction,
    pub run_id: String,
    pub base_sha: String,
    pub worktree: String,
    pub artifact_dir: String,
    pub iterations: u32,
    pub final_diff: String,
    pub applied: bool,
    pub stop_reason: Option<StopReason>,
    pub changed_files: Vec<String>,
    pub scope: Option<ScopeSnapshot>,
    pub verify: Option<VerifySnapshot>,
    pub judge: Option<JudgeSnapshot>,
    pub builder: BuilderSnapshot,
}

fn load_project_lessons() -> anyhow::Result<Option<String>> {
    let path = std::path::Path::new(".bob").join("lessons.md");
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)?;
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    if text.len() > 16_000 {
        anyhow::bail!(".bob/lessons.md is too large; keep it curated and under 16KB");
    }
    let hits = crate::safety::scan(text);
    if !hits.is_empty() {
        anyhow::bail!("secret-scan flagged .bob/lessons.md: {:?}", hits);
    }
    Ok(Some(text.to_string()))
}

fn spec_with_lessons(spec: &str, lessons: Option<&str>) -> String {
    match lessons {
        Some(l) => format!("{spec}\n\n## PROJECT LESSONS\n{l}"),
        None => spec.to_string(),
    }
}

fn build_prompt(opts: &RunOpts, critique: Option<&str>, lessons: Option<&str>) -> String {
    let mut p = format!("## TASK / SPEC\n{}\n", opts.spec);
    if !opts.context_files.is_empty() {
        p.push_str("\n## CONTEXT FILES\n");
        for f in &opts.context_files {
            p.push_str(&format!("- {}\n", f.display()));
        }
    }
    if let Some(l) = lessons {
        p.push_str(&format!("\n## PROJECT LESSONS\n{l}\n"));
    }
    if let Some(c) = critique {
        p.push_str(&format!(
            "\n## PREVIOUS ATTEMPT WAS REJECTED — FIX THIS\n{c}\n"
        ));
    }
    p
}

fn verdict_name(v: Verdict) -> &'static str {
    match v {
        Verdict::Pass => "pass",
        Verdict::Fail => "fail",
        Verdict::Uncertain => "uncertain",
    }
}

fn result_next_action(
    status: RunStatus,
    applied: bool,
    stop_reason: Option<StopReason>,
) -> NextAction {
    if status == RunStatus::Converged {
        return if applied {
            NextAction::Done
        } else {
            NextAction::ReviewCandidate
        };
    }
    if status == RunStatus::NeedsReview {
        return NextAction::ReviewCandidate;
    }
    match stop_reason {
        Some(StopReason::ScopeExceeded) => NextAction::SplitTask,
        Some(StopReason::EmptyDiffAfterCritique) => NextAction::EscalateModel,
        Some(StopReason::RepeatedVerifyFailure) => NextAction::RetryWithVerifyFailure,
        Some(StopReason::JudgeRejected) => NextAction::RetryWithJudgeCritique,
        _ => NextAction::HumanDecisionRequired,
    }
}

pub fn should_try_next_model(res: &RunResult) -> bool {
    res.status == RunStatus::NotConverged
        && matches!(
            res.stop_reason,
            Some(StopReason::EmptyDiffAfterCritique | StopReason::RepeatedVerifyFailure)
        )
}

fn should_try_next_model_after_error(err: &anyhow::Error) -> bool {
    let s = err.to_string();
    s.contains("builder ") || s.contains("spawning builder")
}

fn model_label(model: &Option<String>) -> String {
    model
        .clone()
        .unwrap_or_else(|| "(opencode default)".to_string())
}

pub async fn run_opencode_with_fallbacks(
    cfg: &Config,
    opts: RunOpts,
    model_override: Option<String>,
    fallback_overrides: Vec<String>,
) -> anyhow::Result<RunResult> {
    // Fallbacks are escalation, not load balancing. Bob only advances when the
    // builder is stuck or errored before producing a usable candidate; verify
    // and scope failures remain evidence for the orchestrator/Hector.
    let sequence = cfg
        .builder
        .model_sequence(model_override.as_deref(), &fallback_overrides);
    let mut fallback_history = Vec::new();
    let mut last_err: Option<anyhow::Error> = None;

    for (idx, model_sel) in sequence.iter().enumerate() {
        let resolved_model = cfg.builder.resolved_model(model_sel.as_deref());
        if idx > 0 {
            eprintln!(
                "bob: retrying with fallback model {}",
                model_label(&resolved_model)
            );
        }
        let builder = Opencode {
            cmd: cfg.builder.cmd.clone(),
            timeout: Duration::from_secs(cfg.builder.timeout_secs),
            args: cfg.builder.opencode_args(model_sel.as_deref()),
        };
        let judge = Abe {
            cmd: cfg.judge.cmd.clone(),
            mode: cfg.judge.mode,
            timeout: Duration::from_secs(cfg.judge.timeout_secs),
        };
        let mut attempt_opts = opts.clone();
        attempt_opts.run_id = if idx == 0 {
            opts.run_id.clone()
        } else {
            format!("{}-fb{idx}", opts.run_id)
        };
        attempt_opts.builder_model = resolved_model.clone();

        match run(cfg, attempt_opts, &builder, &judge).await {
            Ok(mut res) => {
                res.builder.fallbacks_tried = fallback_history.clone();
                if should_try_next_model(&res) && idx + 1 < sequence.len() {
                    fallback_history.push(format!(
                        "{}: {:?}",
                        model_label(&resolved_model),
                        res.stop_reason
                    ));
                    continue;
                }
                res.builder.fallbacks_tried = fallback_history;
                return Ok(res);
            }
            Err(e) if should_try_next_model_after_error(&e) && idx + 1 < sequence.len() => {
                fallback_history.push(format!("{}: {e}", model_label(&resolved_model)));
                last_err = Some(e);
                continue;
            }
            Err(e) => return Err(e),
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("no builder model attempts ran")))
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
    let lessons = load_project_lessons()?;
    let judge_spec = spec_with_lessons(&opts.spec, lessons.as_deref());
    let ws = Workspace::create(&opts.run_id)?;
    let base_sha = ws.base_sha().to_string();
    let deadline = Instant::now() + Duration::from_secs(cfg.loop_cfg.max_walltime_secs);

    let mut state = LoopState {
        index: 0,
        max_iterations: cfg.loop_cfg.max_iterations,
        had_critique: false,
        last_verify_fail: None,
        uncertain_streak: 0,
        walltime_exceeded: false,
    };
    let mut critique: Option<String> = None;
    let mut final_diff = String::new();
    let mut applied = false;
    let mut stop_reason = None;
    let mut status = RunStatus::NotConverged;
    let mut last_scope: Option<scope::ScopeReport> = None;
    let mut last_verify: Option<VerifySnapshot> = None;
    let mut last_judge: Option<JudgeSnapshot> = None;
    let mut builder_snapshot = BuilderSnapshot {
        model: opts.builder_model.clone(),
        stdout_tail: String::new(),
        stderr_tail: String::new(),
        failure_kind: String::new(),
        fallbacks_tried: vec![],
    };

    loop {
        state.walltime_exceeded = Instant::now() >= deadline;
        let prompt = build_prompt(&opts, critique.as_deref(), lessons.as_deref());

        // BUILD
        let builder_out: BuilderOutcome = match builder.build(&prompt, ws.path()).await {
            Ok(out) => out,
            Err(e) => {
                if !opts.keep_worktree {
                    let _ = ws.cleanup();
                }
                return Err(e);
            }
        };
        builder_snapshot.stdout_tail = builder_out.stdout_tail;
        builder_snapshot.stderr_tail = builder_out.stderr_tail;
        builder_snapshot.failure_kind = builder_out.failure_kind;
        let diff = ws.capture_diff()?;
        final_diff = diff.clone();

        // STEP OUTCOME
        let step = if diff.trim().is_empty() {
            StepOutcome::EmptyDiff
        } else {
            let sr = scope::check(&diff, &cfg.scope);
            last_scope = Some(sr.clone());
            if !sr.within {
                StepOutcome::scope_exceeded(&sr.detail)
            } else {
                let vr = run_gates(&cfg.verify.cmds, ws.path());
                last_verify = Some(VerifySnapshot {
                    passed: vr.passed,
                    cmd: vr.cmd.clone(),
                    output_tail: crate::builder::tail(&vr.output, 4000),
                });
                if !vr.passed {
                    StepOutcome::VerifyFailed { output: vr.output }
                } else {
                    match judge.judge(&judge_spec, &diff, &vr.output).await {
                        Ok(o) => {
                            if cfg.judge.policy == JudgePolicy::Advisory
                                && !o.critique.trim().is_empty()
                            {
                                eprintln!("abe advisory (non-blocking):\n{}\n", o.critique);
                            }
                            last_judge = Some(JudgeSnapshot {
                                policy: cfg.judge.policy,
                                verdict: verdict_name(o.verdict).into(),
                                critique: o.critique.clone(),
                            });
                            StepOutcome::Judged {
                                verdict: o.verdict,
                                critique: o.critique,
                            }
                        }
                        Err(e) => {
                            let detail = e.to_string();
                            if cfg.judge.policy == JudgePolicy::Advisory {
                                eprintln!("abe advisory unavailable (non-blocking): {detail}");
                            }
                            last_judge = Some(JudgeSnapshot {
                                policy: cfg.judge.policy,
                                verdict: "unavailable".into(),
                                critique: detail.clone(),
                            });
                            StepOutcome::judge_unavailable(&detail)
                        }
                    }
                }
            }
        };

        let verdict_label = match &step {
            StepOutcome::EmptyDiff => "empty-diff".to_string(),
            StepOutcome::ScopeExceeded { detail } => format!("scope-exceeded: {detail}"),
            StepOutcome::VerifyFailed { .. } => "verify-failed".to_string(),
            StepOutcome::JudgeUnavailable { detail } => format!("judge-unavailable: {detail}"),
            StepOutcome::Judged { verdict, critique } => {
                if critique.trim().is_empty() {
                    format!("{verdict:?}")
                } else {
                    format!("{verdict:?}\n\n{critique}")
                }
            }
        };
        let _ = crate::report::write_artifacts(
            std::path::Path::new(&cfg.artifacts.dir),
            &opts.run_id,
            state.index,
            &prompt,
            &final_diff,
            &verdict_label,
        );

        let action = next_action(&state, &step, cfg.judge.policy);

        // update streaks AFTER deciding
        if let StepOutcome::Judged {
            verdict: Verdict::Uncertain,
            ..
        } = &step
        {
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
                let diff_hits = crate::safety::scan(&final_diff);
                if !diff_hits.is_empty() {
                    eprintln!(
                        "secret-scan flagged the candidate diff; NOT applying: {diff_hits:?}"
                    );
                    status = RunStatus::NotConverged;
                    stop_reason = Some(StopReason::SecretScanBlocked);
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
                break;
            }
            LoopAction::Continue { critique: c } => {
                critique = Some(c);
                state.had_critique = true;
                state.index += 1;
            }
            LoopAction::Stop { reason } => {
                if reason == StopReason::RepeatedUncertain
                    && last_verify.as_ref().is_some_and(|v| v.passed)
                {
                    status = RunStatus::NeedsReview;
                }
                stop_reason = Some(reason);
                break;
            }
        }
    }

    let worktree = ws.path().to_string_lossy().to_string();
    let artifact_dir = std::path::Path::new(&cfg.artifacts.dir)
        .join(&opts.run_id)
        .to_string_lossy()
        .to_string();
    let changed_files = last_scope
        .as_ref()
        .map(|s| s.changed_files.clone())
        .unwrap_or_default();
    let scope = last_scope.as_ref().map(|s| ScopeSnapshot {
        within: s.within,
        files: s.files,
        lines: s.lines,
        detail: s.detail.clone(),
    });
    let next_action = result_next_action(status, applied, stop_reason);
    let result = RunResult {
        status,
        next_action,
        run_id: opts.run_id.clone(),
        base_sha,
        worktree,
        artifact_dir,
        iterations: state.index + 1,
        final_diff,
        applied,
        stop_reason,
        changed_files,
        scope,
        verify: last_verify,
        judge: last_judge,
        builder: builder_snapshot,
    };
    if opts.keep_worktree {
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
        LoopState {
            index,
            max_iterations: max,
            had_critique: index > 0,
            last_verify_fail: None,
            uncertain_streak: 0,
            walltime_exceeded: false,
        }
    }

    #[test]
    fn lessons_are_added_to_builder_prompt_and_judge_spec() {
        let opts = RunOpts {
            spec: "fix the route".into(),
            context_files: vec![],
            apply: false,
            keep_worktree: false,
            run_id: "r".into(),
            builder_model: None,
        };
        let prompt = build_prompt(&opts, None, Some("- Do not edit focused tests."));
        assert!(prompt.contains("## PROJECT LESSONS"));
        assert!(prompt.contains("Do not edit focused tests"));

        let judge_spec = spec_with_lessons(&opts.spec, Some("- Keep API shape stable."));
        assert!(judge_spec.contains("fix the route"));
        assert!(judge_spec.contains("Keep API shape stable"));
    }

    #[test]
    fn pass_verdict_applies() {
        let s = state(0, 3);
        let step = StepOutcome::judged(true, Verdict::Pass, "ok");
        assert!(matches!(
            next_action(&s, &step, JudgePolicy::RetryOnFail),
            LoopAction::Apply
        ));
    }
    #[test]
    fn verify_fail_continues_with_verify_output() {
        let s = state(0, 3);
        let step = StepOutcome::verify_failed("cargo test failed: X");
        match next_action(&s, &step, JudgePolicy::RetryOnFail) {
            LoopAction::Continue { critique } => assert!(critique.contains("cargo test failed")),
            other => panic!("expected Continue, got {other:?}"),
        }
    }
    #[test]
    fn repeated_identical_verify_failure_stops() {
        let mut s = state(1, 3);
        s.last_verify_fail = Some("same error".to_string());
        let step = StepOutcome::verify_failed("same error");
        assert!(matches!(
            next_action(&s, &step, JudgePolicy::RetryOnFail),
            LoopAction::Stop {
                reason: StopReason::RepeatedVerifyFailure
            }
        ));
    }
    #[test]
    fn empty_diff_after_critique_stops() {
        let s = state(1, 3); // had_critique == true
        let step = StepOutcome::empty_diff();
        assert!(matches!(
            next_action(&s, &step, JudgePolicy::RetryOnFail),
            LoopAction::Stop {
                reason: StopReason::EmptyDiffAfterCritique
            }
        ));
    }
    #[test]
    fn advisory_fail_verdict_still_applies() {
        let s = state(0, 3);
        let step = StepOutcome::judged(true, Verdict::Fail, "missing X");
        assert!(matches!(
            next_action(&s, &step, JudgePolicy::Advisory),
            LoopAction::Apply
        ));
    }
    #[test]
    fn blocking_fail_verdict_stops() {
        let s = state(0, 3);
        let step = StepOutcome::judged(true, Verdict::Fail, "missing X");
        assert!(matches!(
            next_action(&s, &step, JudgePolicy::Blocking),
            LoopAction::Stop {
                reason: StopReason::JudgeRejected
            }
        ));
    }
    #[test]
    fn retry_on_fail_verdict_continues_with_critique() {
        let s = state(0, 3);
        let step = StepOutcome::judged(true, Verdict::Fail, "missing X");
        match next_action(&s, &step, JudgePolicy::RetryOnFail) {
            LoopAction::Continue { critique } => assert!(critique.contains("missing X")),
            other => panic!("expected Continue, got {other:?}"),
        }
    }
    #[test]
    fn first_uncertain_continues() {
        let s = state(0, 3); // streak 0
        let step = StepOutcome::judged(true, Verdict::Uncertain, "unsure");
        assert!(matches!(
            next_action(&s, &step, JudgePolicy::RetryOnFail),
            LoopAction::Continue { .. }
        ));
    }
    #[test]
    fn vague_uncertain_applies_under_retry_policy() {
        let s = state(0, 3);
        let step = StepOutcome::judged(true, Verdict::Uncertain, "");
        assert!(matches!(
            next_action(&s, &step, JudgePolicy::RetryOnFail),
            LoopAction::Apply
        ));
    }
    #[test]
    fn two_uncertain_in_a_row_stops() {
        let mut s = state(1, 3);
        s.uncertain_streak = 1;
        let step = StepOutcome::judged(true, Verdict::Uncertain, "unsure");
        assert!(matches!(
            next_action(&s, &step, JudgePolicy::RetryOnFail),
            LoopAction::Stop {
                reason: StopReason::RepeatedUncertain
            }
        ));
    }
    #[test]
    fn last_iteration_fail_stops_at_max() {
        let s = state(2, 3); // index 2 is the 3rd (0-based); next would be == max
        let step = StepOutcome::judged(true, Verdict::Fail, "still wrong");
        assert!(matches!(
            next_action(&s, &step, JudgePolicy::RetryOnFail),
            LoopAction::Stop {
                reason: StopReason::MaxIterations
            }
        ));
    }
    #[test]
    fn scope_exceeded_stops() {
        let s = state(0, 3);
        let step = StepOutcome::scope_exceeded("21 files");
        assert!(matches!(
            next_action(&s, &step, JudgePolicy::RetryOnFail),
            LoopAction::Stop {
                reason: StopReason::ScopeExceeded
            }
        ));
    }

    #[test]
    fn fallback_only_on_stuck_results() {
        let mut res = RunResult {
            status: RunStatus::NotConverged,
            next_action: NextAction::EscalateModel,
            run_id: "r".into(),
            base_sha: "b".into(),
            worktree: "w".into(),
            artifact_dir: "a".into(),
            iterations: 1,
            final_diff: String::new(),
            applied: false,
            stop_reason: Some(StopReason::EmptyDiffAfterCritique),
            changed_files: vec![],
            scope: None,
            verify: None,
            judge: None,
            builder: BuilderSnapshot {
                model: None,
                stdout_tail: String::new(),
                stderr_tail: String::new(),
                failure_kind: "ok".into(),
                fallbacks_tried: vec![],
            },
        };
        assert!(should_try_next_model(&res));
        res.stop_reason = Some(StopReason::ScopeExceeded);
        assert!(!should_try_next_model(&res));
    }

    #[test]
    fn needs_review_points_to_candidate_review() {
        assert_eq!(
            result_next_action(
                RunStatus::NeedsReview,
                false,
                Some(StopReason::RepeatedUncertain)
            ),
            NextAction::ReviewCandidate
        );
    }
}

#[cfg(test)]
mod flow_tests {
    use super::*;
    use crate::judge::JudgeOutcome;
    use std::cell::Cell;
    use std::cell::RefCell;
    use std::path::Path;

    // Makes NO change on the first call, then a real change — exercises the
    // empty-diff retry path (the retry trigger under the verify-authority model).
    struct FlakyBuilder {
        calls: Cell<u32>,
    }
    impl Builder for FlakyBuilder {
        async fn build(&self, _p: &str, workdir: &Path) -> anyhow::Result<BuilderOutcome> {
            let n = self.calls.get();
            self.calls.set(n + 1);
            if n >= 1 {
                std::fs::write(workdir.join("out.txt"), "change\n")?;
            }
            Ok(BuilderOutcome {
                failure_kind: "ok".into(),
                ..Default::default()
            })
        }
    }

    struct NoopBuilder;
    impl Builder for NoopBuilder {
        async fn build(&self, _p: &str, _workdir: &Path) -> anyhow::Result<BuilderOutcome> {
            Ok(BuilderOutcome {
                failure_kind: "ok".into(),
                ..Default::default()
            })
        }
    }
    // Always Uncertain. Under verify-authority this verdict is advisory and must
    // NOT block convergence — the test asserts exactly that.
    struct UncertainJudge;
    impl Judge for UncertainJudge {
        async fn judge(&self, _s: &str, _d: &str, _v: &str) -> anyhow::Result<JudgeOutcome> {
            Ok(JudgeOutcome {
                verdict: Verdict::Uncertain,
                critique: "advisory only".into(),
            })
        }
    }

    struct RecordingBuilder {
        calls: Cell<u32>,
        prompts: RefCell<Vec<String>>,
    }
    impl Builder for RecordingBuilder {
        async fn build(&self, p: &str, workdir: &Path) -> anyhow::Result<BuilderOutcome> {
            let n = self.calls.get();
            self.calls.set(n + 1);
            self.prompts.borrow_mut().push(p.to_string());
            std::fs::write(workdir.join("out.txt"), format!("change {n}\n"))?;
            Ok(BuilderOutcome {
                failure_kind: "ok".into(),
                ..Default::default()
            })
        }
    }

    struct FailThenPassJudge {
        calls: Cell<u32>,
    }
    impl Judge for FailThenPassJudge {
        async fn judge(&self, _s: &str, _d: &str, _v: &str) -> anyhow::Result<JudgeOutcome> {
            let n = self.calls.get();
            self.calls.set(n + 1);
            if n == 0 {
                Ok(JudgeOutcome {
                    verdict: Verdict::Fail,
                    critique: "missing edge case".into(),
                })
            } else {
                Ok(JudgeOutcome {
                    verdict: Verdict::Pass,
                    critique: String::new(),
                })
            }
        }
    }

    #[tokio::test]
    async fn empty_diff_retries_then_verify_pass_converges() {
        let _cwd_guard = crate::CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // requires running inside a temp git repo; see worktree::tests helper.
        let tmp = std::env::temp_dir().join(format!("bob-flow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let g = |a: &[&str]| {
            std::process::Command::new("git")
                .args(a)
                .current_dir(&tmp)
                .output()
                .unwrap();
        };
        g(&["init", "-q"]);
        g(&["config", "user.email", "t@t"]);
        g(&["config", "user.name", "t"]);
        std::fs::write(tmp.join("seed.txt"), "x\n").unwrap();
        g(&["add", "."]);
        g(&["commit", "-qm", "init"]);

        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&tmp).unwrap();

        let cfg = crate::config::Config {
            builder: crate::config::BuilderCfg {
                cmd: "opencode".into(),
                timeout_secs: 5,
                args: vec![],
                model: None,
                models: Default::default(),
                fallback_models: vec![],
            },
            judge: crate::config::JudgeCfg {
                cmd: "abe".into(),
                mode: crate::config::JudgeMode::Validate,
                timeout_secs: 600,
                policy: crate::config::JudgePolicy::Advisory,
            },
            verify: crate::config::VerifyCfg {
                cmds: vec!["true".into()],
            }, // gate that passes
            loop_cfg: crate::config::LoopCfg {
                max_iterations: 3,
                max_walltime_secs: 60,
            },
            scope: Default::default(),
            apply: false,
            artifacts: Default::default(),
        };
        let opts = RunOpts {
            spec: "do the thing".into(),
            context_files: vec![],
            apply: false,
            keep_worktree: false,
            run_id: "flow".into(),
            builder_model: None,
        };
        let res = run(
            &cfg,
            opts,
            &FlakyBuilder {
                calls: Cell::new(0),
            },
            &UncertainJudge,
        )
        .await
        .unwrap();

        std::env::set_current_dir(prev).unwrap();
        // iter-0: empty diff -> continue; iter-1: change -> verify gate passes ->
        // converge, despite the judge returning Uncertain (now advisory only).
        assert_eq!(res.status, RunStatus::Converged);
        assert_eq!(res.iterations, 2);
    }

    #[tokio::test]
    async fn retry_on_fail_refeeds_judge_critique() {
        let _cwd_guard = crate::CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("bob-retry-flow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let g = |a: &[&str]| {
            std::process::Command::new("git")
                .args(a)
                .current_dir(&tmp)
                .output()
                .unwrap()
        };
        g(&["init", "-q"]);
        g(&["config", "user.email", "t@t"]);
        g(&["config", "user.name", "t"]);
        std::fs::write(tmp.join("seed.txt"), "x\n").unwrap();
        g(&["add", "."]);
        g(&["commit", "-qm", "init"]);

        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&tmp).unwrap();

        let cfg = crate::config::Config {
            builder: crate::config::BuilderCfg {
                cmd: "opencode".into(),
                timeout_secs: 5,
                args: vec![],
                model: None,
                models: Default::default(),
                fallback_models: vec![],
            },
            judge: crate::config::JudgeCfg {
                cmd: "abe".into(),
                mode: crate::config::JudgeMode::Validate,
                timeout_secs: 600,
                policy: crate::config::JudgePolicy::RetryOnFail,
            },
            verify: crate::config::VerifyCfg {
                cmds: vec!["true".into()],
            },
            loop_cfg: crate::config::LoopCfg {
                max_iterations: 3,
                max_walltime_secs: 60,
            },
            scope: Default::default(),
            apply: false,
            artifacts: Default::default(),
        };
        let opts = RunOpts {
            spec: "do the thing".into(),
            context_files: vec![],
            apply: false,
            keep_worktree: false,
            run_id: "retry-flow".into(),
            builder_model: None,
        };
        let builder = RecordingBuilder {
            calls: Cell::new(0),
            prompts: RefCell::new(vec![]),
        };
        let res = run(
            &cfg,
            opts,
            &builder,
            &FailThenPassJudge {
                calls: Cell::new(0),
            },
        )
        .await
        .unwrap();

        std::env::set_current_dir(prev).unwrap();
        let prompts = builder.prompts.borrow();
        assert_eq!(res.status, RunStatus::Converged);
        assert_eq!(res.iterations, 2);
        assert!(prompts[1].contains("missing edge case"));
        assert_eq!(res.judge.as_ref().unwrap().verdict, "pass");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn non_converged_run_cleans_worktree_by_default() {
        let _cwd_guard = crate::CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("bob-clean-flow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let g = |a: &[&str]| {
            std::process::Command::new("git")
                .args(a)
                .current_dir(&tmp)
                .output()
                .unwrap()
        };
        g(&["init", "-q"]);
        g(&["config", "user.email", "t@t"]);
        g(&["config", "user.name", "t"]);
        std::fs::write(tmp.join("seed.txt"), "x\n").unwrap();
        g(&["add", "."]);
        g(&["commit", "-qm", "init"]);

        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&tmp).unwrap();

        let cfg = crate::config::Config {
            builder: crate::config::BuilderCfg {
                cmd: "opencode".into(),
                timeout_secs: 5,
                args: vec![],
                model: None,
                models: Default::default(),
                fallback_models: vec![],
            },
            judge: crate::config::JudgeCfg {
                cmd: "abe".into(),
                mode: crate::config::JudgeMode::Validate,
                timeout_secs: 600,
                policy: crate::config::JudgePolicy::Advisory,
            },
            verify: crate::config::VerifyCfg { cmds: vec![] },
            loop_cfg: crate::config::LoopCfg {
                max_iterations: 3,
                max_walltime_secs: 60,
            },
            scope: Default::default(),
            apply: false,
            artifacts: Default::default(),
        };
        let opts = RunOpts {
            spec: "do the thing".into(),
            context_files: vec![],
            apply: false,
            keep_worktree: false,
            run_id: "clean-flow".into(),
            builder_model: None,
        };
        let res = run(&cfg, opts, &NoopBuilder, &UncertainJudge)
            .await
            .unwrap();
        let worktree = std::path::PathBuf::from(&res.worktree);

        std::env::set_current_dir(prev).unwrap();
        assert_eq!(res.status, RunStatus::NotConverged);
        assert!(!worktree.exists(), "worktree should be cleaned by default");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
