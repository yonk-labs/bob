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
        };
        let j = to_json(&res);
        assert!(j.contains("\"status\":\"needs_review\""));
        assert!(j.contains("\"next_action\":\"review_candidate\""));
        assert!(j.contains("\"iterations\":2"));
        assert!(j.contains("\"changed_files\":[\"src/lib.rs\"]"));
        assert!(j.contains("\"fallbacks_tried\":[\"qwen: EmptyDiffAfterCritique\"]"));
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
}
