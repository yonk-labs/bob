mod builder;
mod campaign;
mod cli;
mod config;
mod doctor;
mod engine;
mod init;
mod judge;
mod mcp;
mod model_stats;
mod report;
mod safety;
mod scope;
mod verify;
mod worktree;

use clap::Parser;
use cli::{Cli, Command};

static BUILD_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn load_run_json(artifacts_dir: &str, run_id: &str) -> anyhow::Result<serde_json::Value> {
    let path = std::path::Path::new(artifacts_dir)
        .join(run_id)
        .join("run.json");
    let text = std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("no run.json for {run_id} at {}: {e}", path.display()))?;
    Ok(serde_json::from_str(&text)?)
}

/// Build the machine-readable tier→endpoint map for `bob models --json`.
/// Serializes only what's actually configured: tiers with no models are
/// omitted, and base_url is null unless the roster entry is the explicit
/// `Full` form (bob never guesses endpoints — same resolution `doctor
/// --probe` uses via `entry_base_url`).
fn models_json(cfg: &config::Config) -> serde_json::Value {
    let tiers_cfg = &cfg.builder.tiers;
    let mut tiers = serde_json::Map::new();
    for tier in ["cheap", "medium", "large", "frontier"] {
        let models = tiers_cfg.models_for(tier);
        if !models.is_empty() {
            tiers.insert(tier.to_string(), serde_json::json!(models));
        }
    }
    let default_tier = tiers_cfg
        .any_configured()
        .then(|| tiers_cfg.default_tier.clone());

    let mut models = serde_json::Map::new();
    for (name, def) in &cfg.builder.models {
        models.insert(
            name.clone(),
            serde_json::json!({
                "id": def.id(),
                "base_url": cfg.builder.entry_base_url(Some(name)),
            }),
        );
    }

    serde_json::json!({
        "default_model": cfg.builder.model,
        "default_tier": default_tier,
        "tiers": tiers,
        "models": models,
    })
}

fn replay_run(cfg: &config::Config, run_id: &str) -> anyhow::Result<(serde_json::Value, bool)> {
    let run = load_run_json(&cfg.artifacts.dir, run_id)?;
    let base_sha = run["base_sha"].as_str().unwrap_or_default().to_string();
    let diff = run["final_diff"].as_str().unwrap_or_default().to_string();
    if diff.trim().is_empty() {
        anyhow::bail!("run {run_id} has an empty final_diff — nothing to replay");
    }
    let cmds: Vec<String> = run["verify_cmds"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let cmds = if cmds.is_empty() {
        cfg.verify.cmds.clone()
    } else {
        cmds
    };
    let repo = std::env::current_dir()?;
    let vr = worktree::replay_verify_at_with_setup(
        &repo,
        &base_sha,
        run_id,
        &diff,
        &cmds,
        &cfg.worktree.setup_cmds,
    )?;
    println!(
        "bob: replay-verify {} for run {run_id} ({} gate(s))",
        if vr.passed { "PASSED" } else { "FAILED" },
        cmds.len()
    );
    Ok((run, vr.passed))
}

#[cfg(test)]
pub(crate) static CWD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Cli::parse();
    // Reap orphans on every invocation. Fast (only scans /proc when stale pids
    // exist). Catches opencode from prior runs whose parent bob was SIGKILLed.
    let _ = builder::reap_orphans();
    match args.command {
        Command::Doctor { probe } => doctor::run(probe),
        Command::Models { json } => {
            let cfg = config::Config::load(args.config.as_deref())?;
            if json {
                println!("{}", models_json(&cfg));
                return Ok(());
            }
            let default = cfg.builder.model.as_deref();
            if cfg.builder.models.is_empty() {
                println!("No model roster configured (builder.models).");
                println!("Default: {}", default.unwrap_or("(opencode's own default)"));
            } else {
                println!("Builder model roster (builder.models):");
                for (name, def) in &cfg.builder.models {
                    let star = if Some(name.as_str()) == default {
                        "  *default"
                    } else {
                        ""
                    };
                    println!("  {name:<14} {}{star}", def.id());
                }
                if let Some(d) = default {
                    if !cfg.builder.models.contains_key(d) {
                        println!("Default '{d}' is a raw id (not in the roster).");
                    }
                } else {
                    println!("No default set (builder.model) — opencode uses its own default.");
                }
            }
            Ok(())
        }
        Command::Build {
            task,
            spec,
            files,
            max_iters,
            model,
            run_id,
            fallback_models,
            verify_cmds,
            allow_paths,
            max_changed_files,
            max_changed_lines,
            judge_policy,
            judge_mode,
            tier,
            skip_escalation,
            json,
            apply,
            keep,
            keep_worktree,
        } => {
            let mut cfg = config::Config::load(args.config.as_deref())?;
            if let Some(m) = max_iters {
                cfg.loop_cfg.max_iterations = m;
            }
            if !verify_cmds.is_empty() {
                cfg.verify.cmds = verify_cmds;
            }
            if !allow_paths.is_empty() {
                cfg.scope.allow_paths = allow_paths.clone();
            }
            if let Some(n) = max_changed_files {
                cfg.scope.max_changed_files = n;
            }
            if let Some(n) = max_changed_lines {
                cfg.scope.max_changed_lines = n;
            }
            if let Some(p) = judge_policy {
                cfg.judge.policy = p;
            }
            if let Some(m) = judge_mode {
                cfg.judge.mode = m;
            }
            let spec_text = match spec {
                Some(ref p) => {
                    if crate::safety::risky_filename(&p.to_string_lossy()) {
                        anyhow::bail!("refusing: spec path looks sensitive: {}", p.display());
                    }
                    std::fs::read_to_string(p)?
                }
                None => task.clone(),
            };
            let apply = apply || cfg.apply;
            let run_id = match run_id {
                Some(id) => {
                    engine::validate_run_id(&id).map_err(|e| anyhow::anyhow!(e))?;
                    engine::check_run_id_collision(&cfg.artifacts.dir, &id)
                        .map_err(|e| anyhow::anyhow!(e))?;
                    id
                }
                None => format!(
                    "{}-{}",
                    std::process::id(),
                    BUILD_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                ),
            };
            let opts = engine::RunOpts {
                spec: spec_text,
                context_files: files,
                apply,
                keep_worktree: keep || keep_worktree,
                run_id,
                builder_model: None,
                editable_paths: allow_paths.clone(),
                tier,
            };
            let res = engine::run_opencode_with_fallbacks(
                &cfg,
                opts,
                model,
                fallback_models,
                skip_escalation,
            )
            .await?;
            if json {
                // Machine contract: JSON only, diff is in `final_diff`.
                println!("{}", crate::report::to_json(&res));
            } else {
                crate::report::print(&res);
                if !res.applied {
                    println!("{}", res.final_diff);
                }
            }
            // Exit non-zero when the loop did not converge so automation/CI can detect it.
            if res.status != engine::RunStatus::Converged {
                use std::io::Write;
                std::io::stdout().flush().ok();
                std::process::exit(1);
            }
            Ok(())
        }
        Command::Mcp => mcp::serve().await,
        Command::Init => init::run(),
        Command::Gc { dry_run } => {
            let report = worktree::gc(dry_run)?;
            let verb = if report.dry_run {
                "would remove"
            } else {
                "removed"
            };
            for path in &report.worktrees {
                println!("{verb} worktree {}", path.display());
            }
            for branch in &report.branches {
                println!("{verb} branch {branch}");
            }
            if report.worktrees.is_empty() && report.branches.is_empty() {
                println!("bob gc: nothing to clean");
            }
            Ok(())
        }
        Command::Reap => {
            let report = builder::reap_orphans()?;
            println!(
                "reaper: killed {} orphan(s), cleaned {} stale pid file(s)",
                report.orphans_killed, report.cleaned
            );
            Ok(())
        }
        Command::Campaign { file } => {
            let cfg = config::Config::load(args.config.as_deref())?;
            let report = campaign::run_file(&file, &cfg).await?;
            println!("{}", campaign::to_json(&report));
            if report.status != "completed" {
                std::process::exit(1);
            }
            Ok(())
        }
        Command::Stats { reset } => {
            if reset {
                match model_stats::StatsStore::reset() {
                    Some(p) => println!(
                        "reset: removed {} — rankings back to cold start",
                        p.display()
                    ),
                    None => println!("nothing to reset (.bob/model-stats.json not found)"),
                }
            } else {
                let weight = config::Config::load(args.config.as_deref())
                    .map(|c| c.builder.reliability_weight)
                    .unwrap_or(0.5);
                model_stats::StatsStore::load().print_summary(weight);
            }
            Ok(())
        }
        Command::Replay { run_id } => {
            let cfg = config::Config::load(args.config.as_deref())?;
            let (_, passed) = replay_run(&cfg, &run_id)?;
            if !passed {
                std::process::exit(1);
            }
            Ok(())
        }
        Command::Apply { run_id } => {
            let cfg = config::Config::load(args.config.as_deref())?;
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
            let patch = std::path::Path::new(&cfg.artifacts.dir)
                .join(&run_id)
                .join("apply.patch");
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
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn models_json_reports_tiers_and_explicit_base_urls() {
        let yaml = r#"
builder:
  cmd: opencode
  model: qwen
  models:
    qwen: { model: "Intel/Qwen3", base_url: "http://host:8000/v1" }
    codex: { model: "gpt-5-codex", base_url: "https://api.openai.com/v1" }
  tiers:
    cheap: [qwen]
    frontier: [codex]
    default_tier: cheap
judge: { cmd: abe }
verify: { cmds: [] }
"#;
        let cfg = serde_yaml::from_str::<config::Config>(yaml).unwrap();
        let v = models_json(&cfg);
        assert_eq!(v["default_model"], "qwen");
        assert_eq!(v["default_tier"], "cheap");
        assert_eq!(v["tiers"]["cheap"], serde_json::json!(["qwen"]));
        assert_eq!(v["tiers"]["frontier"], serde_json::json!(["codex"]));
        assert!(v["tiers"].get("medium").is_none());
        assert!(v["tiers"].get("large").is_none());
        assert_eq!(v["models"]["qwen"]["id"], "Intel/Qwen3");
        assert_eq!(v["models"]["qwen"]["base_url"], "http://host:8000/v1");
        assert_eq!(
            v["models"]["codex"]["base_url"],
            "https://api.openai.com/v1"
        );
    }

    #[test]
    fn models_json_null_base_url_for_raw_id_models() {
        let yaml = r#"
builder:
  cmd: opencode
  models:
    legacy: ollama/Intel/Qwen3-Coder
judge: { cmd: abe }
verify: { cmds: [] }
"#;
        let cfg = serde_yaml::from_str::<config::Config>(yaml).unwrap();
        let v = models_json(&cfg);
        assert_eq!(v["models"]["legacy"]["id"], "ollama/Intel/Qwen3-Coder");
        assert!(v["models"]["legacy"]["base_url"].is_null());
    }

    #[test]
    fn models_json_empty_tiers_and_null_default_tier_when_unconfigured() {
        let yaml = r#"
builder:
  cmd: opencode
judge: { cmd: abe }
verify: { cmds: [] }
"#;
        let cfg = serde_yaml::from_str::<config::Config>(yaml).unwrap();
        let v = models_json(&cfg);
        assert!(v["default_model"].is_null());
        assert!(v["default_tier"].is_null());
        assert_eq!(v["tiers"], serde_json::json!({}));
        assert_eq!(v["models"], serde_json::json!({}));
    }
}
