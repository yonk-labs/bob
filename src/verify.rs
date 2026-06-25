use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone)]
pub struct VerifyResult { pub passed: bool, pub output: String }

/// Run each gate as `sh -c <cmd>` in workdir. First failure stops and is reported.
/// Empty cmds => pass (abe becomes sole gate; caller warns).
pub fn run_gates(cmds: &[String], workdir: &Path) -> VerifyResult {
    if cmds.is_empty() {
        return VerifyResult { passed: true, output: "no verify gates configured".into() };
    }
    for cmd in cmds {
        let out = Command::new("sh").arg("-c").arg(cmd).current_dir(workdir).output();
        match out {
            Ok(o) if o.status.success() => continue,
            Ok(o) => {
                let combined = format!(
                    "gate failed: {cmd}\n--- stdout ---\n{}\n--- stderr ---\n{}",
                    String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
                return VerifyResult { passed: false, output: combined };
            }
            Err(e) => return VerifyResult { passed: false, output: format!("gate '{cmd}' could not run: {e}") },
        }
    }
    VerifyResult { passed: true, output: "all gates passed".into() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn empty_gates_pass() {
        let r = run_gates(&[], Path::new("."));
        assert!(r.passed);
    }
    #[test]
    fn failing_gate_reports_output() {
        let r = run_gates(&["echo boom && exit 1".to_string()], Path::new("."));
        assert!(!r.passed);
        assert!(r.output.contains("boom"));
    }
    #[test]
    fn passing_gate_passes() {
        let r = run_gates(&["true".to_string()], Path::new("."));
        assert!(r.passed);
    }
}
