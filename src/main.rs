#![allow(dead_code)] // interface stubs consumed by later tasks
mod builder;
mod cli;
mod config;
mod doctor;
mod engine;
mod judge;
mod mcp;
mod report;
mod safety;
mod scope;
mod verify;
mod worktree;

use clap::Parser;
use cli::{Cli, Command};

#[cfg(test)]
pub(crate) static CWD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Cli::parse();
    match args.command {
        Command::Doctor => doctor::run(),
        Command::Build { task, spec, files, max_iters, apply, keep } => {
            let mut cfg = config::Config::load(args.config.as_deref())?;
            if let Some(m) = max_iters { cfg.loop_cfg.max_iterations = m; }
            let spec_text = match spec {
                Some(p) => std::fs::read_to_string(p)?,
                None => task.clone(),
            };
            let apply = apply || cfg.apply;
            let builder = builder::Opencode { cmd: cfg.builder.cmd.clone(),
                timeout: std::time::Duration::from_secs(cfg.builder.timeout_secs) };
            let judge = judge::Abe { cmd: cfg.judge.cmd.clone(), mode: cfg.judge.mode,
                timeout: std::time::Duration::from_secs(cfg.judge.timeout_secs) };
            let run_id = format!("{}", std::process::id());
            let opts = engine::RunOpts { spec: spec_text, context_files: files, apply, keep, run_id };
            let res = engine::run(&cfg, opts, &builder, &judge).await?;
            crate::report::print(&res);
            if !res.applied {
                println!("{}", res.final_diff);
            }
            Ok(())
        }
        Command::Mcp => mcp::serve().await,
        Command::Init => { anyhow::bail!("init not yet implemented") }
    }
}
