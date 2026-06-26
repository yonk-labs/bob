use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "bob", about = "Autonomous build-verify-judge loop")]
pub struct Cli {
    /// Path to config (default: ./bob.yaml then ~/.config/bob/config.yaml)
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Run the build-verify-judge loop on a task/spec.
    Build {
        /// Task description (free text).
        task: String,
        #[arg(long)]
        spec: Option<PathBuf>,
        #[arg(long, num_args = 0..)]
        files: Vec<PathBuf>,
        #[arg(long)]
        max_iters: Option<u32>,
        /// Model to build with: a name from builder.models, or a raw provider/model id.
        #[arg(long)]
        model: Option<String>,
        /// Apply the candidate to the working tree on pass (default: propose only).
        #[arg(long)]
        apply: bool,
        /// Keep the worktree + artifacts even on success.
        #[arg(long)]
        keep: bool,
    },
    /// Run the stdio MCP server.
    Mcp,
    /// Write a starter bob.yaml into the current directory.
    Init,
    /// Check git/opencode/abe presence and config validity.
    Doctor,
    /// List the builder model roster (builder.models) and the default.
    Models,
}
