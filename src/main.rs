#![allow(dead_code)] // interface stubs consumed by later tasks
mod builder;
mod cli;
mod config;
mod doctor;
mod judge;
mod safety;
mod verify;
mod worktree;

use clap::Parser;
use cli::{Cli, Command};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Cli::parse();
    match args.command {
        Command::Doctor => doctor::run(),
        Command::Build { .. } => { anyhow::bail!("build not yet implemented") }
        Command::Mcp => { anyhow::bail!("mcp not yet implemented") }
        Command::Init => { anyhow::bail!("init not yet implemented") }
    }
}
