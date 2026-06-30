use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone)]
pub struct VerifyResult {
    pub passed: bool,
    pub output: String,
    pub cmd: Option<String>,
}

/// Run each gate as `sh -c <cmd>` in workdir. First failure stops and is reported.
/// Empty cmds => pass (abe becomes sole gate; caller warns).
pub fn run_gates(cmds: &[String], workdir: &Path) -> VerifyResult {
    if cmds.is_empty() {
        return VerifyResult {
            passed: true,
            output: "no verify gates configured".into(),
            cmd: None,
        };
    }
    for cmd in cmds {
        let out = Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .current_dir(workdir)
            .output();
        match out {
            Ok(o) if o.status.success() => continue,
            Ok(o) => {
                let combined = format!(
                    "gate failed: {cmd}\n--- stdout ---\n{}\n--- stderr ---\n{}",
                    String::from_utf8_lossy(&o.stdout),
                    String::from_utf8_lossy(&o.stderr)
                );
                return VerifyResult {
                    passed: false,
                    output: combined,
                    cmd: Some(cmd.clone()),
                };
            }
            Err(e) => {
                return VerifyResult {
                    passed: false,
                    output: format!("gate '{cmd}' could not run: {e}"),
                    cmd: Some(cmd.clone()),
                }
            }
        }
    }
    VerifyResult {
        passed: true,
        output: "all gates passed".into(),
        cmd: Some(cmds.join(" && ")),
    }
}

/// Pre-flight sanity check: run the verify gate on the *unmodified base tree*
/// before the build loop and diagnose two footguns that otherwise masquerade as
/// "the model failed":
///   1. The gate already PASSES on base → it doesn't test the intended change, so
///      bob would "converge" on an empty diff having built nothing.
///   2. The gate ERRORS (command not found, bad flags) rather than failing tests →
///      it can never pass, so bob loops forever feeding the error back to the builder.
/// A normal test failure on base is EXPECTED (bob builds to turn it green) → no warning.
/// Returns a warning message to surface, or None when the gate looks healthy.
pub fn preflight_diagnose(cmds: &[String], workdir: &Path) -> Option<String> {
    if cmds.is_empty() {
        return None;
    }
    let r = run_gates(cmds, workdir);
    if r.passed {
        return Some(
            "⚠️  verify gate PASSES on the unmodified tree — it likely doesn't test the intended \
             change. bob would 'converge' on an empty diff without building anything. The tests \
             should FAIL on the base and pass once the change is made."
                .to_string(),
        );
    }
    if looks_like_command_error(&r.output) {
        let tail = crate::builder::truncate_chars(&r.output, 700);
        return Some(format!(
            "⚠️  verify gate '{}' ERRORED on the base tree — this looks like a command/usage error, \
             not test failures, so the gate may be UNPASSABLE and bob will loop. Fix the command first.\n{}",
            r.cmd.as_deref().unwrap_or("?"),
            tail
        ));
    }
    None // ordinary test failure on base — expected; bob will build to make it pass
}

/// Heuristic: does this gate output look like the *command itself* failed (missing
/// binary, bad flags, no tests matched) rather than tests running and failing?
fn looks_like_command_error(out: &str) -> bool {
    const MARKERS: &[&str] = &[
        "command not found",
        "no such file or directory",
        "is not recognized",
        "unknown option",
        "unknown argument",
        "only one is allowed",
        "no tests found",
        "no test files found",
        "cannot find module",
        "could not run",
        "permission denied",
        "not executable",
    ];
    let low = out.to_lowercase();
    MARKERS.iter().any(|m| low.contains(m))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn preflight_flags_gate_that_passes_on_base() {
        // gate passes on base => too weak to test a change
        assert!(preflight_diagnose(&["true".into()], Path::new("."))
            .unwrap()
            .contains("PASSES on the unmodified tree"));
    }

    #[test]
    fn preflight_flags_broken_command() {
        // jest's real "both flags" error; exit 1 but it's a usage error, not a test fail
        let cmd = "echo 'Both --runInBand and --maxWorkers were specified, only one is allowed' && exit 1";
        assert!(preflight_diagnose(&[cmd.into()], Path::new("."))
            .unwrap()
            .contains("ERRORED on the base tree"));
    }

    #[test]
    fn preflight_quiet_on_ordinary_test_failure() {
        // tests ran and failed on base => expected; no warning
        let cmd = "echo 'Tests: 1 failed, 3 passed' && exit 1";
        assert!(preflight_diagnose(&[cmd.into()], Path::new(".")).is_none());
    }

    #[test]
    fn preflight_quiet_on_no_gates() {
        assert!(preflight_diagnose(&[], Path::new(".")).is_none());
    }

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
