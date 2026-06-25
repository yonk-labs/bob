use std::path::Path;
use crate::engine::{RunResult, RunStatus};

pub fn to_json(res: &RunResult) -> String {
    let status = match res.status { RunStatus::Converged => "converged",
        RunStatus::NotConverged => "not_converged", RunStatus::Error => "error" };
    let reason = res.stop_reason.as_ref().map(|r| format!("{r:?}")).unwrap_or_default();
    serde_json::json!({
        "status": status,
        "base_sha": res.base_sha,
        "iterations": res.iterations,
        "applied": res.applied,
        "stop_reason": reason,
        "final_diff": res.final_diff,
    }).to_string()
}

pub fn print(res: &RunResult) {
    let s = match res.status { RunStatus::Converged => "CONVERGED",
        RunStatus::NotConverged => "NOT CONVERGED", RunStatus::Error => "ERROR" };
    println!("bob: {s} in {} iteration(s); applied={}", res.iterations, res.applied);
    if let Some(r) = &res.stop_reason { println!("  stop reason: {r:?}"); }
    if !res.applied && res.status == RunStatus::Converged {
        println!("  (propose mode — candidate diff below; re-run with --apply to merge)");
    }
}

pub fn write_artifacts(dir: &Path, run_id: &str, iter: u32,
                       prompt: &str, diff: &str, verdict: &str) -> anyhow::Result<()> {
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
    use crate::engine::{RunResult, RunStatus};

    #[test]
    fn json_has_status_and_iterations() {
        let res = RunResult { status: RunStatus::Converged, base_sha: "abc".into(),
            iterations: 2, final_diff: "diff".into(), applied: true, stop_reason: None };
        let j = to_json(&res);
        assert!(j.contains("\"status\":\"converged\""));
        assert!(j.contains("\"iterations\":2"));
    }

    #[test]
    fn writes_artifact_files() {
        let tmp = std::env::temp_dir().join(format!("bob-art-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        write_artifacts(&tmp, "r1", 0, "P", "D", "Pass").unwrap();
        assert!(tmp.join("r1/iter-0/prompt.txt").exists());
        assert_eq!(std::fs::read_to_string(tmp.join("r1/iter-0/diff.patch")).unwrap(), "D");
    }
}
