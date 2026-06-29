use crate::builder::{Builder, BuilderOutcome};
use crate::config::{Config, JudgePolicy};
use crate::judge::{Abe, Judge, Verdict};
use crate::scope;
use crate::verify::run_gates;
use crate::worktree::{ApplyOutcome, Workspace};
use std::path::PathBuf;
use std::path::{Component, Path};
use std::time::{Duration, Instant};

/// Check if a path looks like a test file. Matches common conventions:
/// tests/, test/, __tests__/ dirs, and *_test.*, *.test.*, *.spec.* suffixes.
fn is_test_path(path: &str) -> bool {
    let p = Path::new(path);
    p.components().any(|c| matches!(c, Component::Normal(s) if s == "tests" || s == "test" || s == "__tests__"))
        || path.ends_with("_test.rs")
        || path.ends_with("_test.js")
        || path.ends_with("_test.py")
        || path.ends_with(".test.js")
        || path.ends_with(".test.ts")
        || path.ends_with(".spec.js")
        || path.ends_with(".spec.ts")
}

/// Discard any test-file changes (modified or new) in the worktree. Bob may
/// only edit production code — test files are hector's frozen contract. If the
/// model modified tests (common — models love "fixing" tests), those changes
/// are reverted before the scope check runs. This prevents scope-exceeded stops
/// that block the retry/abe feedback loop.
fn freeze_untracked_test_files(workdir: &Path) {
    // Commit untracked test files into the worktree base BEFORE the builder runs.
    // These are hector's frozen contracts — they should be part of the base so
    // capture_diff doesn't see them as "new files" (→ scope-exceeded).
    if let Ok(out) = std::process::Command::new("git")
        .args(["ls-files", "--others", "--exclude-standard"])
        .current_dir(workdir)
        .output()
    {
        let untracked: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|f| is_test_path(f))
            .map(|s| s.to_string())
            .collect();
        if !untracked.is_empty() {
            eprintln!(
                "bob: freezing {} untracked test file(s) into worktree base: {}",
                untracked.len(),
                untracked.join(", ")
            );
            let _ = std::process::Command::new("git")
                .args(["add", "--"])
                .args(&untracked)
                .current_dir(workdir)
                .status();
            let _ = std::process::Command::new("git")
                .args(["commit", "-q", "-m", "bob: freeze reference test files"])
                .current_dir(workdir)
                .status();
        }
    }
}

fn reset_test_files(workdir: &Path, base_sha: &str) {
    let mut test_files: Vec<String> = Vec::new();

    // Find tracked test files that differ from base_sha (the model modified them)
    if let Ok(out) = std::process::Command::new("git")
        .args(["diff", "--name-only", base_sha])
        .current_dir(workdir)
        .output()
    {
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            if is_test_path(line) {
                test_files.push(line.to_string());
            }
        }
    }

    if test_files.is_empty() {
        return;
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
}

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
    pub editable_paths: Vec<String>,
    /// Tier for this run: cheap | large | frontier. Overrides config default_tier.
    pub tier: Option<String>,
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
            editable_paths: self.editable_paths.clone(),
            tier: self.tier.clone(),
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

/// Build the judge spec: task + lessons + context file contents. Giving abe the
/// actual test code and spec lets it make a definitive verdict (Pass/Fail)
/// instead of defaulting to Uncertain because it can't verify correctness from
/// the diff alone.
fn build_judge_spec(
    spec: &str,
    lessons: Option<&str>,
    context_files: &[std::path::PathBuf],
    _workdir: &std::path::Path,
) -> String {
    let mut out = spec_with_lessons(spec, lessons);
    for f in context_files {
        // Read from the main repo (bob's cwd), NOT the worktree — context files
        // are reference material that lives in the user's workspace, not
        // necessarily committed or present in the isolated worktree.
        if let Ok(content) = std::fs::read_to_string(f) {
            // Truncate large files to avoid context bloat (char-safe).
            let truncated = crate::builder::truncate_chars(&content, 4000);
            out.push_str(&format!(
                "\n\n## REFERENCE FILE: {}\n```\n{}\n```",
                f.display(),
                truncated
            ));
        }
    }
    out
}

fn build_prompt(opts: &RunOpts, critique: Option<&str>, lessons: Option<&str>) -> String {
    let mut p = format!(
        "## TASK / SPEC\n{}\n\n\
         ## RULES\n\
         - Edit ONLY the paths in editable_paths. Any other path is rejected.\n\
         - You are NOT authorized to modify test files. Tests are frozen contracts owned by hector.\n\
         - If you believe a test is INCORRECT, do NOT modify it. Implement the code to match the SPEC.\n\
         - After implementing, if you have concerns about the test, add a `## CONCERNS` section at the\n\
           end of your response explaining what you think is wrong and why. This will be reported back.\n\
         - Match the API signature the test expects exactly. The spec is the contract.\n\
         - Do NOT modify test files (tests/, *_test.*, *.test.*). Tests are frozen.\n\
         - Implement to make the test/gate pass — don't change the contract.\n",
        opts.spec
    );
    if !opts.editable_paths.is_empty() {
        p.push_str("\n## EDITABLE PATHS\n");
        for path in &opts.editable_paths {
            p.push_str(&format!("- {path}\n"));
        }
    }
    if !opts.context_files.is_empty() {
        p.push_str("\n## CONTEXT FILES (read-only)\n");
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
    // A builder that timed out, crashed, failed to spawn, or errored at the model
    // API is a *per-model* failure — escalate to the next model/tier rather than
    // killing the whole run. Match every builder's error vocabulary, not just
    // opencode's: goose ("goose timed out after", "goose exited with status",
    // "spawning goose"), opencode ("builder timed out", "spawning builder"), and
    // thin ("thin builder: model API error"). Orchestration errors (git/worktree)
    // don't use these phrases, so they still fail fast.
    s.contains("timed out")
        || s.contains("exited with status")
        || s.contains("spawning ")
        || s.contains("builder")
        || s.contains("model API error")
}

fn model_label(model: &Option<String>) -> String {
    model
        .clone()
        .unwrap_or_else(|| "(opencode default)".to_string())
}

/// Extract the model name (without provider prefix) for API calls.
/// "ollama/Intel/Qwen3..." → "Intel/Qwen3..."
/// "192.168.1.133/cyankiwi/gemma..." → "cyankiwi/gemma..."
fn extract_model_name(model_id: &str) -> String {
    if let Some(pos) = model_id.find('/') {
        model_id[pos + 1..].to_string()
    } else {
        model_id.to_string()
    }
}

/// Extract base_url from a model id for the thin builder.
/// "ollama/Intel/Qwen3..." → "http://192.168.1.193:8000/v1"
/// "192.168.1.133/cyankiwi/..." → "http://192.168.1.133:8000/v1"
/// "zai-coding-plan/glm-5.2" → (cloud, needs env-based URL)
fn extract_base_url(model_id: &str) -> String {
    if model_id.starts_with("ollama/") {
        crate::model_stats::vllm_url()
    } else if model_id.starts_with("192.168.1.") {
        let ip = model_id.split('/').next().unwrap_or("192.168.1.193");
        format!("http://{ip}:8000/v1")
    } else if model_id.starts_with("minimax") {
        "https://api.minimax.io/v1".into()
    } else if model_id.starts_with("zai") {
        "https://api.z.ai/api/paas/v4".into()
    } else {
        // Fallback: assume local vLLM (env-overridable).
        crate::model_stats::vllm_url()
    }
}

/// Resolve (base_url, api_model, api_key) for a thin/goose builder. An explicit
/// roster entry (`models: { name: { base_url, ... } }`) wins; otherwise we fall
/// back to deriving them from the model-id prefix (the legacy hardcoded map).
/// This is what lets a configured model avoid the baked-in IPs entirely.
fn resolve_endpoint(
    cfg: &Config,
    sel: Option<&str>,
    model_id: &str,
) -> (String, String, Option<String>) {
    let base_url = cfg
        .builder
        .entry_base_url(sel)
        .unwrap_or_else(|| extract_base_url(model_id));
    let api_model = cfg
        .builder
        .entry_api_model(sel)
        .unwrap_or_else(|| extract_model_name(model_id));
    let api_key = cfg
        .builder
        .entry_api_key_env(sel)
        .or_else(|| extract_api_key_env(model_id))
        .and_then(|env| std::env::var(&env).ok());
    (base_url, api_model, api_key)
}

/// Extract the env var name for the API key (if any) from a model id.
fn extract_api_key_env(model_id: &str) -> Option<String> {
    if model_id.starts_with("minimax") {
        Some("MINIMAX_API_KEY".into())
    } else if model_id.starts_with("zai") && !model_id.contains("coding-plan") {
        Some("ZAI_API_KEY".into())
    } else {
        None // local models don't need keys
    }
}

/// Combine the tier-derived model chain with explicit per-call overrides:
/// `--model` leads, the tier chain follows, `--fallback-model` entries trail.
/// Order-preserving dedup keeps the first occurrence so a forced model that
/// also lives in a tier isn't attempted twice.
fn apply_overrides(
    base: Vec<Option<String>>,
    model_override: Option<String>,
    fallback_overrides: Vec<String>,
) -> Vec<Option<String>> {
    let mut chain: Vec<Option<String>> = Vec::new();
    chain.extend(model_override.map(Some));
    chain.extend(base);
    chain.extend(fallback_overrides.into_iter().map(Some));
    let mut seen: Vec<Option<String>> = Vec::new();
    chain.retain(|item| {
        if seen.contains(item) {
            false
        } else {
            seen.push(item.clone());
            true
        }
    });
    chain
}

/// Resolve the ordered model-attempt sequence.
/// - `skip_escalation`: exactly one attempt — the `--model` override, else the
///   config default (`None` = the builder's own default). No tiers, no
///   fallbacks; a single-entry sequence can't fail over.
/// - otherwise: the tier-derived `tier_sequence` with overrides applied
///   (`--model` leads, `--fallback-model` trails). If that's empty — a config
///   with no tiers — fall back to one default attempt so bob still runs once.
fn resolve_sequence(
    skip_escalation: bool,
    tier_sequence: Vec<Option<String>>,
    model_override: Option<String>,
    fallback_overrides: Vec<String>,
    config_model: Option<String>,
) -> Vec<Option<String>> {
    if skip_escalation {
        return vec![model_override.or(config_model)];
    }
    let seq = apply_overrides(tier_sequence, model_override, fallback_overrides);
    if seq.is_empty() {
        vec![None]
    } else {
        seq
    }
}

pub async fn run_opencode_with_fallbacks(
    cfg: &Config,
    opts: RunOpts,
    // An explicit `--model` (CLI/MCP) is tried FIRST, ahead of the tier chain.
    // `--fallback-model` entries are appended after the tier chain.
    model_override: Option<String>,
    fallback_overrides: Vec<String>,
    // When true, run exactly one model (the override or config default) — no
    // tier escalation, no fallbacks.
    skip_escalation: bool,
) -> anyhow::Result<RunResult> {
    // Fallbacks are escalation, not load balancing. Bob only advances when the
    // builder is stuck or errored before producing a usable candidate; verify
    // and scope failures remain evidence for the orchestrator/Hector.
    //
    // TIER RESOLUTION: determine the starting tier for this slice, then build
    // an ordered tier list (starting tier → next tiers for escalation).
    // Within each tier, models are ranked by stats (success × speed).
    //
    // Tier selection: slice.tier (per-call override) > config default_tier > "cheap"
    let slice_tier = opts.tier.as_deref()
        .unwrap_or(&cfg.builder.tiers.default_tier);
    let tiers_to_try = cfg.builder.tiers.ordered_tiers(slice_tier);

    // ADAPTIVE: for each tier, rank models by historical performance.
    // Dead/slow models sink to the bottom of their tier.
    let stats = crate::model_stats::StatsStore::load();

    let sequence: Vec<Option<String>> = tiers_to_try
        .iter()
        .flat_map(|tier| {
            let models = cfg.builder.tiers.models_for(tier);
            if models.is_empty() {
                return Vec::new();
            }
            // Resolve aliases to ids, rank by stats, preserve first-position default.
            let ids: Vec<String> = models
                .iter()
                .filter_map(|m| cfg.builder.resolved_model(Some(m)))
                .collect();
            let ranked = stats.rank(&ids);
            // Map ranked ids back to aliases, preserve config-order for ties.
            let mut ordered: Vec<String> = Vec::new();
            for ranked_id in &ranked {
                for m in models {
                    let resolved = cfg.builder.resolved_model(Some(m));
                    if resolved.as_deref() == Some(ranked_id.as_str())
                        && !ordered.contains(m)
                    {
                        ordered.push(m.clone());
                        break;
                    }
                }
            }
            // Default (first in config) leads within tier
            if let Some(default) = models.first() {
                if !ordered.iter().any(|x| x == default) {
                    ordered.insert(0, default.clone());
                }
            }
            ordered.into_iter().map(Some).collect::<Vec<_>>()
        })
        .collect();

    let tiers_configured = cfg.builder.tiers.any_configured();
    // Warn on the genuinely-misconfigured case: tiers declared but none usable.
    if !skip_escalation && tiers_configured && sequence.is_empty() {
        eprintln!(
            "bob: no models configured for tier '{}' (or any escalation tier). Check bob.yaml tiers config.",
            slice_tier
        );
    }
    let sequence = resolve_sequence(
        skip_escalation,
        sequence,
        model_override,
        fallback_overrides,
        cfg.builder.model.clone(),
    );

    eprintln!(
        "bob: tier='{}' chain (ranked by stats): {:?}",
        slice_tier,
        sequence
            .iter()
            .map(|a| cfg.builder.resolved_model(a.as_deref()))
            .collect::<Vec<_>>()
    );

    let mut fallback_history = Vec::new();
    let mut last_err: Option<anyhow::Error> = None;
    let mut current_tier_idx: usize = 0;

    for (idx, model_sel) in sequence.iter().enumerate() {
        let resolved_model = cfg.builder.resolved_model(model_sel.as_deref());
        let model_id = resolved_model.as_deref().unwrap_or("default");

        // HEALTH CHECK: skip dead endpoints instantly (3s probe, not 300s timeout)
        if !crate::model_stats::StatsStore::health_check(model_id) {
            eprintln!(
                "bob: skipping {} — health check failed (endpoint unreachable)",
                model_label(&resolved_model)
            );
            fallback_history.push(format!(
                "{}: health_check_failed",
                model_label(&resolved_model)
            ));
            continue;
        }

        // TIER ESCALATION: if escalation_policy=tier and we're about to move
        // to a new tier, log the transition.
        let alias = model_sel.as_deref().unwrap_or("");
        let model_tier = tiers_to_try
            .iter()
            .position(|t| cfg.builder.tiers.models_for(t).iter().any(|m| m == alias))
            .unwrap_or(0);
        if model_tier > current_tier_idx {
            let from_tier = tiers_to_try.get(current_tier_idx).cloned().unwrap_or_else(|| "?".into());
            let to_tier = tiers_to_try.get(model_tier).cloned().unwrap_or_else(|| "?".into());
            eprintln!("bob: escalating from tier '{from_tier}' to tier '{to_tier}'");
            current_tier_idx = model_tier;
        }

        if idx > 0 {
            eprintln!(
                "bob: retrying with fallback model {}",
                model_label(&resolved_model)
            );
        }

        // ADAPTIVE TIMEOUT: use historical avg × 2, clamped [30s, 180s]
        let model_stats = stats.get(model_id);
        let adaptive = model_stats.adaptive_timeout();
        let configured = Duration::from_secs(cfg.builder.timeout_secs);
        // Adaptive timing may EXTEND patience for known-slow models but must never
        // shrink the user's configured budget — agentic builders (goose/opencode)
        // legitimately need minutes, and min() silently capped 600s configs at ~90s.
        let builder_timeout = adaptive.max(configured);
        eprintln!(
            "bob: timeout configured={}s adaptive={}s effective={}s",
            configured.as_secs(),
            adaptive.as_secs(),
            builder_timeout.as_secs()
        );

        let mut attempt_opts = opts.clone();
        attempt_opts.run_id = if idx == 0 {
            opts.run_id.clone()
        } else {
            format!("{}-fb{idx}", opts.run_id)
        };
        attempt_opts.builder_model = resolved_model.clone();

        // Construct the right builder type for this tier.
        // cheap → ThinBuilder (curl, minimal context) or whatever's configured
        // frontier → Opencode (full agent loop) by default
        let tier_name = tiers_to_try.get(current_tier_idx)
            .map(|s| s.as_str())
            .unwrap_or("cheap");
        // Tier-less config → honor builder.cmd so `cmd: goose` / `cmd: thin`
        // route to their builders instead of being forced through opencode.
        // Unknown cmds (custom opencode wrappers) fall through to the `_` arm below.
        let builder_kind = if tiers_configured {
            cfg.builder.tiers.builder_for(tier_name)
        } else {
            cfg.builder.cmd.as_str()
        };
        eprintln!("bob: builder='{}' for tier='{}'", builder_kind, tier_name);

        let builder: crate::builder::BuilderKind = match builder_kind {
            "thin" => {
                let (base_url, api_model, api_key) =
                    resolve_endpoint(cfg, model_sel.as_deref(), model_id);
                crate::builder::BuilderKind::Thin(crate::builder::ThinBuilder {
                    model_id: api_model,
                    base_url,
                    api_key,
                    timeout: builder_timeout,
                })
            }
            "goose" => {
                let (base_url, api_model, api_key) =
                    resolve_endpoint(cfg, model_sel.as_deref(), model_id);
                crate::builder::BuilderKind::Goose(crate::builder::GooseBuilder {
                    cmd: "goose".to_string(),
                    model: api_model,
                    provider: "openai".to_string(),
                    timeout: builder_timeout,
                    base_url: Some(base_url),
                    api_key,
                })
            }
            _ => crate::builder::BuilderKind::Opencode(crate::builder::Opencode {
                cmd: cfg.builder.cmd.clone(),
                timeout: builder_timeout,
                args: cfg.builder.opencode_args(model_sel.as_deref()),
                run_id: Some(attempt_opts.run_id.clone()),
            }),
        };
        let judge = Abe {
            cmd: cfg.judge.cmd.clone(),
            mode: cfg.judge.mode,
            timeout: Duration::from_secs(cfg.judge.timeout_secs),
        };

        let run_start = std::time::Instant::now();
        match run(cfg, attempt_opts, &builder, &judge).await {
            Ok(mut res) => {
                let latency = run_start.elapsed().as_secs_f64();
                let success = res.status == crate::engine::RunStatus::Converged;
                crate::model_stats::StatsStore::record_run(model_id, latency, success);
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
                let latency = run_start.elapsed().as_secs_f64();
                crate::model_stats::StatsStore::record_run(model_id, latency, false);
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
    let ws = Workspace::create(&opts.run_id)?;
    let base_sha = ws.base_sha().to_string();
    let judge_spec = build_judge_spec(&opts.spec, lessons.as_deref(), &opts.context_files, ws.path());
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

    // Freeze untracked test files BEFORE the builder loop starts. These files
    // were created by hector but never committed. If we don't freeze them now,
    // capture_diff's `git add -A` will stage the model's MODIFIED version of
    // the test as a "new file" → scope-exceeded. By committing the original
    // hector version before the builder runs, the test is part of the base and
    // any model modifications show up as tracked changes (which reset_test_files
    // handles inside the loop).
    freeze_untracked_test_files(ws.path());

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

        // Discard any test-file changes the model made. Bob may only edit
        // production code. If the model modified or created test files, revert
        // them so the scope check and verify gate see only src/ changes.
        reset_test_files(ws.path(), ws.base_sha());

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
    fn builder_failures_escalate_orchestration_errors_dont() {
        let esc = |m: &str| should_try_next_model_after_error(&anyhow::anyhow!("{m}"));
        // every builder's timeout/crash/spawn vocabulary must escalate to next model
        assert!(esc("goose timed out after 600s"));
        assert!(esc("goose exited with status 1; stderr:\n..."));
        assert!(esc("spawning goose 'goose': No such file"));
        assert!(esc("builder timed out after 600s"));
        assert!(esc("spawning builder 'opencode': No such file"));
        assert!(esc("thin builder: model API error: 503"));
        // orchestration failures should NOT escalate (next model won't help)
        assert!(!esc("failed to create worktree"));
        assert!(!esc("git checkout failed"));
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
            editable_paths: vec![],
            tier: None,
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
            next_action(&s, &step, JudgePolicy::Advisory),
            LoopAction::Stop {
                reason: StopReason::ScopeExceeded
            }
        ));
    }

    fn s(v: &str) -> Option<String> {
        Some(v.to_string())
    }

    #[test]
    fn override_model_leads_and_dedups_tier_chain() {
        // base tier chain: cheap → large
        let base = vec![s("qwen"), s("llama")];
        let out = apply_overrides(base, Some("llama".into()), vec![]);
        // forced llama leads; the tier's llama is deduped away (not run twice)
        assert_eq!(out, vec![s("llama"), s("qwen")]);
    }

    #[test]
    fn override_appends_extra_fallbacks_after_tier_chain() {
        let base = vec![s("qwen")];
        let out = apply_overrides(base, None, vec!["codex".into(), "qwen".into()]);
        // tier chain first, extra fallback trails, duplicate qwen deduped
        assert_eq!(out, vec![s("qwen"), s("codex")]);
    }

    #[test]
    fn no_overrides_preserves_tier_chain() {
        let base = vec![s("qwen"), s("llama")];
        let out = apply_overrides(base.clone(), None, vec![]);
        assert_eq!(out, base);
    }

    #[test]
    fn skip_escalation_with_model_is_a_single_attempt() {
        // A tier chain + fallbacks that would normally escalate...
        let base = vec![s("qwen"), s("llama")];
        let out = resolve_sequence(true, base, Some("codex".into()), vec!["fb".into()], Some("cfg".into()));
        // ...is reduced to exactly the --model override. Length 1 ⇒ no fail-over.
        assert_eq!(out, vec![s("codex")]);
    }

    #[test]
    fn skip_escalation_without_model_uses_config_default_then_builder_default() {
        // --skip-escalation alone: config default, still one attempt.
        assert_eq!(
            resolve_sequence(true, vec![s("qwen")], None, vec![], Some("cfg".into())),
            vec![s("cfg")]
        );
        // no override and no config model ⇒ None (builder's own default), one attempt.
        assert_eq!(
            resolve_sequence(true, vec![s("qwen")], None, vec![], None),
            vec![None]
        );
    }

    #[test]
    fn without_skip_escalation_keeps_full_chain() {
        // Override leads, tier chain follows — the normal escalating sequence.
        let out = resolve_sequence(false, vec![s("qwen")], Some("codex".into()), vec![], None);
        assert_eq!(out, vec![s("codex"), s("qwen")]);
        // Tier-less config falls back to one default attempt (legacy path).
        assert_eq!(resolve_sequence(false, vec![], None, vec![], None), vec![None]);
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
            Ok(BuilderOutcome::default())
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
                tiers: Default::default(),
                escalation_policy: "tier".into(),
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
            editable_paths: vec![],
            tier: None,
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
                tiers: Default::default(),
                escalation_policy: "tier".into(),
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
            editable_paths: vec![],
            tier: None,
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
                tiers: Default::default(),
                escalation_policy: "tier".into(),
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
            editable_paths: vec![],
            tier: None,
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
