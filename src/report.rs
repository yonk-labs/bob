use crate::engine::{RunResult, RunStatus};
use std::path::Path;

pub fn to_json(res: &RunResult) -> String {
    let status = match res.status {
        RunStatus::Converged => "converged",
        RunStatus::NeedsReview => "needs_review",
        RunStatus::NotConverged => "not_converged",
        RunStatus::Error => "error",
    };
    let reason = res
        .stop_reason
        .as_ref()
        .map(|r| format!("{r:?}"))
        .unwrap_or_default();
    serde_json::json!({
        "status": status,
        "next_action": res.next_action.as_str(),
        "run_id": &res.run_id,
        "base_sha": &res.base_sha,
        "worktree": &res.worktree,
        "artifact_dir": &res.artifact_dir,
        "iterations": res.iterations,
        "applied": res.applied,
        "stop_reason": reason,
        "changed_files": &res.changed_files,
        "reset_test_files": &res.reset_test_files,
        "context_est_tokens": res.context_est_tokens,
        "prompt_est_tokens": &res.prompt_est_tokens,
        "verify_cmds": &res.verify_cmds,
        "scope": res.scope.as_ref().map(|s| serde_json::json!({
            "within": s.within,
            "files": s.files,
            "lines": s.lines,
            "detail": &s.detail,
        })),
        "verify": res.verify.as_ref().map(|v| serde_json::json!({
            "passed": v.passed,
            "cmd": &v.cmd,
            "output_tail": &v.output_tail,
        })),
        "judge": res.judge.as_ref().map(|j| serde_json::json!({
            "policy": j.policy.as_str(),
            "verdict": &j.verdict,
            "critique": &j.critique,
        })),
        "builder": {
            "model": &res.builder.model,
            "stdout_tail": &res.builder.stdout_tail,
            "stderr_tail": &res.builder.stderr_tail,
            "failure_kind": &res.builder.failure_kind,
            "fallbacks_tried": &res.builder.fallbacks_tried,
        },
        "final_diff": &res.final_diff,
    })
    .to_string()
}

pub fn print(res: &RunResult) {
    let s = match res.status {
        RunStatus::Converged => "CONVERGED",
        RunStatus::NeedsReview => "NEEDS REVIEW",
        RunStatus::NotConverged => "NOT CONVERGED",
        RunStatus::Error => "ERROR",
    };
    println!(
        "bob: {s} in {} iteration(s); applied={}",
        res.iterations, res.applied
    );
    println!("  next action: {}", res.next_action.as_str());
    if let Some(r) = &res.stop_reason {
        println!("  stop reason: {r:?}");
    }
    if !res.builder.fallbacks_tried.is_empty() {
        println!(
            "  fallbacks tried: {}",
            res.builder.fallbacks_tried.join(" | ")
        );
    }
    if !res.applied && res.status == RunStatus::Converged {
        println!("  (propose mode — candidate diff below; re-run with --apply to merge)");
    }
}

pub fn write_artifacts(
    dir: &Path,
    run_id: &str,
    iter: u32,
    prompt: &str,
    diff: &str,
    verdict: &str,
) -> anyhow::Result<()> {
    let d = dir.join(run_id).join(format!("iter-{iter}"));
    std::fs::create_dir_all(&d)?;
    std::fs::write(d.join("prompt.txt"), prompt)?;
    std::fs::write(d.join("diff.patch"), diff)?;
    std::fs::write(d.join("verdict.txt"), verdict)?;
    Ok(())
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{BuilderSnapshot, NextAction, RunResult, RunStatus, VerifySnapshot};

    #[test]
    fn json_has_status_and_iterations() {
        let res = RunResult {
            status: RunStatus::NeedsReview,
            next_action: NextAction::ReviewCandidate,
            run_id: "r1".into(),
            base_sha: "abc".into(),
            worktree: ".bob/worktrees/r1".into(),
            artifact_dir: ".bob/runs/r1".into(),
            iterations: 2,
            final_diff: "diff".into(),
            applied: false,
            stop_reason: None,
            changed_files: vec!["src/lib.rs".into()],
            scope: None,
            verify: Some(VerifySnapshot {
                passed: true,
                cmd: Some("cargo test".into()),
                output_tail: "ok".into(),
            }),
            judge: None,
            builder: BuilderSnapshot {
                model: Some("qwen".into()),
                stdout_tail: String::new(),
                stderr_tail: String::new(),
                failure_kind: "ok".into(),
                fallbacks_tried: vec!["qwen: EmptyDiffAfterCritique".into()],
            },
            reset_test_files: vec![],
            context_est_tokens: 0,
            prompt_est_tokens: vec![],
            verify_cmds: vec![],
        };
        let j = to_json(&res);
        assert!(j.contains("\"status\":\"needs_review\""));
        assert!(j.contains("\"next_action\":\"review_candidate\""));
        assert!(j.contains("\"iterations\":2"));
        assert!(j.contains("\"changed_files\":[\"src/lib.rs\"]"));
        assert!(j.contains("\"fallbacks_tried\":[\"qwen: EmptyDiffAfterCritique\"]"));
    }

    /// CROSS-REPO CONTRACT: hector::planner::review_text routes on these exact
    /// strings from bob's JSON (it lowercases stop_reason). Renaming a
    /// RunStatus / NextAction / StopReason variant silently breaks hector's
    /// review — change both repos together. If this test fails, update
    /// hector/src/planner.rs::review_text too.
    #[test]
    fn cross_repo_status_string_contract() {
        use crate::engine::StopReason;
        assert_eq!(NextAction::SplitTask.as_str(), "split_task");
        assert_eq!(NextAction::ReviewCandidate.as_str(), "review_candidate");
        assert_eq!(
            NextAction::RetryWithVerifyFailure.as_str(),
            "retry_with_verify_failure"
        );
        // stop_reason is emitted via {:?}; hector lowercases it → "scopeexceeded".
        assert_eq!(
            format!("{:?}", StopReason::ScopeExceeded).to_lowercase(),
            "scopeexceeded"
        );
        let mk = |status, next| RunResult {
            status,
            next_action: next,
            run_id: "r".into(),
            base_sha: "b".into(),
            worktree: "w".into(),
            artifact_dir: "a".into(),
            iterations: 1,
            final_diff: String::new(),
            applied: false,
            stop_reason: None,
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
            reset_test_files: vec![],
            context_est_tokens: 0,
            prompt_est_tokens: vec![],
            verify_cmds: vec![],
        };
        assert!(to_json(&mk(RunStatus::Converged, NextAction::Done))
            .contains("\"status\":\"converged\""));
        assert!(
            to_json(&mk(RunStatus::NeedsReview, NextAction::ReviewCandidate))
                .contains("\"status\":\"needs_review\"")
        );
    }

    #[test]
    fn writes_artifact_files() {
        let tmp = std::env::temp_dir().join(format!("bob-art-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        write_artifacts(&tmp, "r1", 0, "P", "D", "Pass").unwrap();
        assert!(tmp.join("r1/iter-0/prompt.txt").exists());
        assert_eq!(
            std::fs::read_to_string(tmp.join("r1/iter-0/diff.patch")).unwrap(),
            "D"
        );
    }

    #[test]
    fn append_event_writes_jsonl() {
        let tmp = std::env::temp_dir().join(format!("bob-ev-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        append_event(
            &tmp,
            "r1",
            serde_json::json!({"event": "verify", "passed": true}),
        );
        append_event(
            &tmp,
            "r1",
            serde_json::json!({"event": "judge", "verdict": "pass"}),
        );
        let text = std::fs::read_to_string(tmp.join("r1/events.jsonl")).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["event"], "verify");
        assert!(first["ts"].as_u64().unwrap() > 0);
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
