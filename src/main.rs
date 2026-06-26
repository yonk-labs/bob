#![allow(dead_code)] // interface stubs consumed by later tasks
mod builder;
mod cli;
mod config;
mod doctor;
mod engine;
mod init;
mod judge;
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
                    let star = if Some(name.as_str()) == default { "  *default" } else { "" };
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
            Ok(())
        }
        Command::Build { task, spec, files, max_iters, model, apply, keep } => {
            let mut cfg = config::Config::load(args.config.as_deref())?;
            if let Some(m) = max_iters { cfg.loop_cfg.max_iterations = m; }
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
            let builder = builder::Opencode { cmd: cfg.builder.cmd.clone(),
                timeout: std::time::Duration::from_secs(cfg.builder.timeout_secs),
                args: cfg.builder.opencode_args(model.as_deref()) };
            let judge = judge::Abe { cmd: cfg.judge.cmd.clone(), mode: cfg.judge.mode,
                timeout: std::time::Duration::from_secs(cfg.judge.timeout_secs) };
            let run_id = format!("{}-{}", std::process::id(),
                BUILD_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
            let opts = engine::RunOpts { spec: spec_text, context_files: files, apply, keep, run_id };
            let res = engine::run(&cfg, opts, &builder, &judge).await?;
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
    }
}
