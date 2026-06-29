use crate::config::{JudgeMode, JudgePolicy};
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
        /// Fallback model to try if the selected model errors or stalls. Repeat for a chain.
        #[arg(long = "fallback-model")]
        fallback_models: Vec<String>,
        /// Override verify gate command for this run. Repeat for multiple gates.
        #[arg(long = "verify")]
        verify_cmds: Vec<String>,
        /// Restrict this run to paths with this prefix. Repeat for multiple paths.
        #[arg(long = "allow-path")]
        allow_paths: Vec<String>,
        /// Override max changed files for this run.
        #[arg(long)]
        max_changed_files: Option<usize>,
        /// Override max changed lines for this run.
        #[arg(long)]
        max_changed_lines: Option<usize>,
        /// Judge behavior after verify passes: advisory, blocking, retry_on_fail.
        #[arg(long)]
        judge_policy: Option<JudgePolicy>,
        /// Judge mode: validate (1 reviewer, fast) or debate (2+ reviewers, deep).
        #[arg(long)]
        judge_mode: Option<JudgeMode>,
        /// Tier: cheap | large | frontier. Overrides bob.yaml default_tier.
        #[arg(long)]
        tier: Option<String>,
        /// Apply the candidate to the working tree on pass (default: propose only).
        #[arg(long)]
        apply: bool,
        /// Keep the worktree even after the run ends. Artifacts are always kept.
        #[arg(long)]
        keep: bool,
        /// Keep the worktree even after the run ends. Artifacts are always kept.
        #[arg(long)]
        keep_worktree: bool,
    },
    /// Run the stdio MCP server.
    Mcp,
    /// Write a starter bob.yaml into the current directory.
    Init,
    /// Check git/opencode/abe presence and config validity.
    Doctor,
    /// List the builder model roster (builder.models) and the default.
    Models,
    /// Remove stale bob worktrees and bob/* branches.
    Gc {
        /// Show what would be removed without deleting anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Reap orphaned opencode processes from prior bob runs. Kills any
    /// opencode whose parent is dead or not a current bob. Run on startup
    /// automatically; manual use to clean up stuck processes.
    Reap,
    /// Run a serial campaign file made of Bob-sized slices.
    Campaign {
        /// YAML or JSON campaign file.
        #[arg(long)]
        file: PathBuf,
    },
    /// Show model performance stats (latency, success rate, adaptive ranking).
    Stats,
}
