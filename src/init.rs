use std::collections::BTreeMap;

use crate::config::{JudgeMode, JudgePolicy};

fn which(cmd: &str) -> bool {
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    let paths = std::env::split_paths(&path_var);

    let exts = if cfg!(target_os = "windows") {
        std::env::var("PATHEXT")
            .ok()
            .map(|p| {
                p.split(';')
                    .filter(|s| !s.is_empty())
                    .map(|s| s.strip_prefix('.').unwrap_or(s).to_string())
                    .collect::<Vec<String>>()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    for dir in paths {
        let path = dir.join(cmd);
        if path.is_file() {
            return true;
        }
        for ext in &exts {
            let mut path_with_ext = path.clone();
            path_with_ext.set_extension(ext);
            if path_with_ext.is_file() {
                return true;
            }
        }
    }
    false
}

fn ask(prompt: &str, default: &str) -> String {
    print!("{} [{}]: ", prompt, default);
    std::io::Write::flush(&mut std::io::stdout()).ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).ok();
    let trimmed = line.trim();
    if trimmed.is_empty() {
        default.to_string()
    } else {
        trimmed.to_string()
    }
}

fn ask_bool(prompt: &str, default: bool) -> bool {
    let d = if default { "Y/n" } else { "y/N" };
    print!("{} [{}]: ", prompt, d);
    std::io::Write::flush(&mut std::io::stdout()).ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).ok();
    let trimmed = line.trim().to_lowercase();
    if trimmed.is_empty() {
        default
    } else {
        trimmed == "y" || trimmed == "yes"
    }
}

fn detect_git() -> bool {
    which("git")
}

fn detect_abe() -> bool {
    which("abe")
}

fn detect_opencode() -> bool {
    which("opencode")
}

/// Make sure `.bob/` (bob's worktrees + run artifacts) can never be committed or
/// staged by other tools. Idempotent: does nothing if `.gitignore` already ignores
/// `.bob` in any common form (`/.bob`, `.bob/`, `.bob`). Creates `.gitignore` with a
/// single `/.bob` line if it doesn't exist yet; otherwise appends `/.bob` on its own
/// line, preserving existing content.
fn ensure_bob_gitignored(dir: &std::path::Path) -> std::io::Result<()> {
    if crate::doctor::gitignore_ignores_bob(dir) {
        return Ok(());
    }
    crate::doctor::append_to_gitignore(dir, &["/.bob"])
}

pub fn run() -> anyhow::Result<()> {
    if !detect_git() {
        eprintln!("[ERROR] git not found on PATH");
        eprintln!("Install git: https://git-scm.com/downloads");
        anyhow::bail!("git required");
    }

    println!("=== Bob Interactive Installer ===");
    println!();

    // Build config step by step
    let builder_cmd = if detect_opencode() {
        println!("[ok] opencode found on PATH");
        "opencode".to_string()
    } else {
        println!("[MISSING] opencode not found on PATH");
        println!("Official install: curl -fsSL https://opencode.ai/install | bash");
        println!("Alternative: npm install -g opencode-ai");
        println!("Alternative: brew install anomalyco/tap/opencode");
        println!();
        ask("Builder command (default: opencode)", "opencode")
    };

    let builder_model = {
        println!();
        let has_models = ask_bool("Configure named model roster (builder.models)", false);
        if !has_models {
            println!("Skipping roster — opencode will use its own default model.");
            None
        } else {
            let mut models = BTreeMap::new();
            loop {
                let name = ask("Model name (e.g. qwen, gpt-4)", "");
                if name.is_empty() {
                    break;
                }
                let id = ask("Model ID (e.g. ollama/Intel/Qwen3-Coder)", "");
                if id.is_empty() {
                    break;
                }
                models.insert(name, id);
                if !ask_bool("Add another model", false) {
                    break;
                }
            }
            if models.is_empty() {
                println!("No models configured.");
                None
            } else {
                let default = ask("Default model name (from roster above)", "");
                if default.is_empty() {
                    None
                } else {
                    Some((default, models))
                }
            }
        }
    };

    let builder_timeout = {
        let d = 600u64;
        let s = ask("Builder timeout (seconds, default: 600)", &d.to_string());
        s.parse().unwrap_or(d)
    };

    let builder_args = {
        println!();
        let s = ask("Extra builder args (space-separated, default: none)", "");
        if s.is_empty() {
            Vec::new()
        } else {
            s.split_whitespace().map(|s| s.to_string()).collect()
        }
    };

    let judge_cmd = {
        if detect_abe() {
            println!("[ok] abe found on PATH");
            "abe".to_string()
        } else {
            println!("[MISSING] abe not found on PATH (optional — judge is advisory)");
            ask("Judge command (default: abe)", "abe")
        }
    };

    let judge_mode = {
        let d = "validate";
        let s = ask("Judge mode: validate | debate (default: validate)", d);
        match s.to_lowercase().as_str() {
            "debate" => JudgeMode::Debate,
            _ => JudgeMode::Validate,
        }
    };

    let judge_timeout = {
        let d = 600u64;
        let s = ask("Judge timeout (seconds, default: 600)", &d.to_string());
        s.parse().unwrap_or(d)
    };

    let verify_cmds = {
        println!();
        println!("Verify commands (objective gates — all must pass):");
        let mut cmds = Vec::new();
        loop {
            let s = ask("  Add verify command (empty to finish)", "");
            if s.is_empty() {
                break;
            }
            cmds.push(s);
        }
        if cmds.is_empty() {
            println!("[WARN] No verify commands — bob will converge on first diff (no guardrail)");
        }
        cmds
    };

    let max_iterations = {
        let d = 3u32;
        let s = ask("Max iterations (default: 3)", &d.to_string());
        s.parse().unwrap_or(d)
    };

    let max_walltime = {
        let d = 1800u64;
        let s = ask("Max walltime (seconds, default: 1800)", &d.to_string());
        s.parse().unwrap_or(d)
    };

    let max_changed_files = {
        let d = 20usize;
        let s = ask("Max changed files (default: 20)", &d.to_string());
        s.parse().unwrap_or(d)
    };

    let max_changed_lines = {
        let d = 800usize;
        let s = ask("Max changed lines (default: 800)", &d.to_string());
        s.parse().unwrap_or(d)
    };

    let allow_paths = {
        println!();
        println!("Allow paths (restrict which paths may change; empty =Anywhere):");
        let mut paths = Vec::new();
        loop {
            let s = ask("  Add allow path (empty to finish)", "");
            if s.is_empty() {
                break;
            }
            paths.push(s);
        }
        paths
    };

    let apply_default = ask_bool("Apply by default (skip propose step)", false);

    let artifacts_dir = {
        let d = ".bob/runs";
        let s = ask("Artifacts directory (default: .bob/runs)", d);
        if s.is_empty() {
            d.to_string()
        } else {
            s
        }
    };

    // Build config struct, then serialize
    let cfg = crate::config::Config {
        builder: crate::config::BuilderCfg {
            cmd: builder_cmd,
            timeout_secs: builder_timeout,
            model: builder_model.as_ref().map(|(d, _)| d.clone()),
            models: builder_model
                .as_ref()
                .map(|(_, m)| {
                    m.iter()
                        .map(|(k, v)| (k.clone(), crate::config::ModelDef::Id(v.clone())))
                        .collect()
                })
                .unwrap_or_default(),
            tiers: Default::default(),
            escalation_policy: "tier".into(),
            reliability_weight: 0.5,
            pin: vec![],
            exclude: vec![],
            goose_toolshim: false,
            idle_stall_secs: 120,
            args: builder_args,
        },
        judge: crate::config::JudgeCfg {
            cmd: judge_cmd,
            mode: judge_mode,
            timeout_secs: judge_timeout,
            policy: JudgePolicy::Advisory,
        },
        verify: crate::config::VerifyCfg {
            cmds: verify_cmds,
            replay: true,
            focused_cmds: vec![],
        },
        loop_cfg: crate::config::LoopCfg {
            max_iterations,
            max_walltime_secs: max_walltime,
        },
        scope: crate::config::ScopeCfg {
            max_changed_files,
            max_changed_lines,
            allow_paths,
        },
        apply: apply_default,
        artifacts: crate::config::ArtifactsCfg { dir: artifacts_dir },
        context: crate::config::ContextCfg::default(),
        worktree: Default::default(),
    };

    let yaml = serde_yaml::to_string(&cfg).expect("config serializes");

    let path = std::path::PathBuf::from("bob.yaml");
    if path.exists() {
        anyhow::bail!("bob.yaml already exists here — not overwriting (edit it, or `rm bob.yaml` to regenerate)");
    }
    std::fs::write(&path, yaml)?;
    println!();
    println!("[DONE] Wrote config to ./bob.yaml");

    if let Err(e) = ensure_bob_gitignored(std::path::Path::new(".")) {
        println!("[WARN] failed to update .gitignore: {e}");
    } else {
        println!("[ok] .gitignore ignores /.bob (worktrees + run artifacts)");
    }

    println!("Next: run `bob doctor` to verify tools + config.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn which_detects_present_cmd() {
        assert!(which("echo"));
    }

    #[test]
    fn which_detects_missing_cmd() {
        assert!(!which("nonexistent-command-12345"));
    }

    #[test]
    fn detect_git_works() {
        // Just verify the function doesn't panic
        let _ = detect_git();
    }

    fn tmp_dir(label: &str) -> std::path::PathBuf {
        // Atomic counter, not the clock — same-tick parallel tests collided (see doctor.rs tmp()).
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("bob-init-{label}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn ensure_bob_gitignored_creates_appends_and_is_idempotent() {
        let dir = tmp_dir("gitignore");

        // Fresh create: no .gitignore yet.
        assert!(!dir.join(".gitignore").exists());
        ensure_bob_gitignored(&dir).unwrap();
        let text = std::fs::read_to_string(dir.join(".gitignore")).unwrap();
        assert_eq!(text, "/.bob\n");

        // Append-to-existing: pre-existing content without a .bob entry.
        std::fs::write(dir.join(".gitignore"), "/target\n").unwrap();
        ensure_bob_gitignored(&dir).unwrap();
        let text = std::fs::read_to_string(dir.join(".gitignore")).unwrap();
        assert_eq!(text, "/target\n/.bob\n");

        // Idempotent: running again must not duplicate the entry.
        ensure_bob_gitignored(&dir).unwrap();
        let text = std::fs::read_to_string(dir.join(".gitignore")).unwrap();
        assert_eq!(text, "/target\n/.bob\n");

        // Already-ignored via a different form: must not add a duplicate line.
        std::fs::write(dir.join(".gitignore"), "node_modules/\n.bob/\n").unwrap();
        ensure_bob_gitignored(&dir).unwrap();
        let text = std::fs::read_to_string(dir.join(".gitignore")).unwrap();
        assert_eq!(text, "node_modules/\n.bob/\n");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_yaml_roundtrip() {
        let yaml = r#"
builder:
  cmd: opencode
  timeout_secs: 600
  args: ["--variant", "high"]
  model: qwen
  models:
    qwen: ollama/Intel/Qwen3-Coder
    minimax: minimax/MiniMax-M3
judge:
  cmd: abe
  mode: validate
  timeout_secs: 600
  policy: advisory
verify:
  cmds: ["cargo test", "cargo clippy"]
loop:
  max_iterations: 3
  max_walltime_secs: 1800
scope:
  max_changed_files: 20
  max_changed_lines: 800
  allow_paths: ["src/"]
apply: false
artifacts:
  dir: .bob/runs
"#;
        let cfg: crate::config::Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.builder.cmd, "opencode");
        assert_eq!(cfg.builder.timeout_secs, 600);
        assert_eq!(cfg.builder.args, vec!["--variant", "high"]);
        assert_eq!(cfg.builder.model, Some("qwen".to_string()));
        assert_eq!(cfg.builder.models.len(), 2);
        assert_eq!(cfg.judge.cmd, "abe");
        assert_eq!(cfg.judge.mode, JudgeMode::Validate);
        assert_eq!(cfg.judge.timeout_secs, 600);
        assert_eq!(cfg.judge.policy, JudgePolicy::Advisory);
        assert_eq!(cfg.verify.cmds, vec!["cargo test", "cargo clippy"]);
        assert_eq!(cfg.loop_cfg.max_iterations, 3);
        assert_eq!(cfg.loop_cfg.max_walltime_secs, 1800);
        assert_eq!(cfg.scope.max_changed_files, 20);
        assert_eq!(cfg.scope.max_changed_lines, 800);
        assert_eq!(cfg.scope.allow_paths, vec!["src/"]);
        assert!(!cfg.apply);
        assert_eq!(cfg.artifacts.dir, ".bob/runs");
    }

    #[test]
    fn config_yaml_serialization_with_special_chars() {
        let cfg = crate::config::Config {
            builder: crate::config::BuilderCfg {
                cmd: "opencode".to_string(),
                timeout_secs: 600,
                model: Some("test".to_string()),
                models: BTreeMap::from([(
                    "qwen".to_string(),
                    crate::config::ModelDef::Id("ollama/Intel/Qwen3-Coder".to_string()),
                )]),
                tiers: Default::default(),
                escalation_policy: "tier".into(),
                reliability_weight: 0.5,
                pin: vec![],
                exclude: vec![],
                goose_toolshim: false,
                idle_stall_secs: 120,
                args: vec!["--variant".to_string(), "high".to_string()],
            },
            judge: crate::config::JudgeCfg {
                cmd: "abe".to_string(),
                mode: JudgeMode::Validate,
                timeout_secs: 600,
                policy: JudgePolicy::Advisory,
            },
            verify: crate::config::VerifyCfg {
                cmds: vec![
                    "cargo test".to_string(),
                    "echo ' special \" chars ".to_string(),
                ],
                replay: true,
                focused_cmds: vec![],
            },
            loop_cfg: crate::config::LoopCfg {
                max_iterations: 3,
                max_walltime_secs: 1800,
            },
            scope: crate::config::ScopeCfg {
                max_changed_files: 20,
                max_changed_lines: 800,
                allow_paths: vec!["src/".to_string(), "test 'path' with spaces".to_string()],
            },
            apply: false,
            artifacts: crate::config::ArtifactsCfg {
                dir: ".bob/runs".to_string(),
            },
            context: crate::config::ContextCfg::default(),
            worktree: Default::default(),
        };
        let yaml = serde_yaml::to_string(&cfg).expect("config serializes");
        let parsed: crate::config::Config = serde_yaml::from_str(&yaml).expect("roundtrip works");
        assert_eq!(parsed.verify.cmds[1], "echo ' special \" chars ");
        assert_eq!(parsed.scope.allow_paths[1], "test 'path' with spaces");
    }
}
