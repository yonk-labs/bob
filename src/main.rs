#![allow(dead_code)] // interface stubs consumed by later tasks
mod builder;
mod campaign;
mod cli;
mod config;
mod doctor;
mod engine;
mod init;
mod judge;
mod model_stats;
mod mcp;
mod report;
mod safety;
mod scope;
mod verify;
mod worktree;

use clap::Parser;
use cli::{Cli, Command};

static BUILD_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(test)]
pub(crate) static CWD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Cli::parse();
    // Reap orphans on every invocation. Fast (only scans /proc when stale pids
    // exist). Catches opencode from prior runs whose parent bob was SIGKILLed.
    let _ = builder::reap_orphans();
    match args.command {
        Command::Doctor => doctor::run(),
        Command::Models => {
            let cfg = config::Config::load(args.config.as_deref())?;
            let default = cfg.builder.model.as_deref();
            if cfg.builder.models.is_empty() {
                println!("No model roster configured (builder.models).");
                println!("Default: {}", default.unwrap_or("(opencode's own default)"));
            } else {
                println!("Builder model roster (builder.models):");
                for (name, id) in &cfg.builder.models {
                    let star = if Some(name.as_str()) == default {
                        "  *default"
                    } else {
                        ""
                    };
                    println!("  {name:<14} {id}{star}");
                }
                if let Some(d) = default {
                    if !cfg.builder.models.contains_key(d) {
                        println!("Default '{d}' is a raw id (not in the roster).");
                    }
                } else {
                    println!("No default set (builder.model) — opencode uses its own default.");
                }
            }
            if cfg.builder.fallback_models.is_empty() {
                println!("Fallbacks: none");
            } else {
                println!("Fallbacks:");
                for name in &cfg.builder.fallback_models {
                    let resolved = cfg
                        .builder
                        .resolved_model(Some(name))
                        .unwrap_or_else(|| name.clone());
                    if cfg.builder.models.contains_key(name) {
                        println!("  {name:<14} {resolved}");
                    } else {
                        println!("  {name:<14} {resolved}  (raw id)");
                    }
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
            fallback_models,
            verify_cmds,
            allow_paths,
            max_changed_files,
            max_changed_lines,
            judge_policy,
            judge_mode,
            tier,
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
            let run_id = format!(
                "{}-{}",
                std::process::id(),
                BUILD_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            );
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
            let res =
                engine::run_opencode_with_fallbacks(&cfg, opts, model, fallback_models).await?;
            crate::report::print(&res);
            if !res.applied {
                println!("{}", res.final_diff);
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
        Command::Stats => {
            let stats = model_stats::StatsStore::load();
            stats.print_summary();
            Ok(())
        }
    }
}
