//! MCP stdio server exposing a `build` tool so agents can invoke the
//! build-verify-judge loop inline. Thin wrapper over engine::run.

static MCP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

use crate::{config::Config, engine, report};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::transport::stdio;
use rmcp::{schemars, tool, tool_handler, tool_router, ServerHandler, ServiceExt};
use serde::Deserialize;

#[derive(Clone)]
pub struct BobServer {
    tool_router: ToolRouter<Self>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BuildParams {
    /// Task description / spec text to implement.
    pub task: String,
    /// Optional explicit spec text (overrides task as the spec body).
    #[serde(default)]
    pub spec: Option<String>,
    /// Context files to include in the prompt.
    #[serde(default)]
    pub files: Option<Vec<String>>,
    /// Override config max_iterations for this run.
    #[serde(default)]
    pub max_iters: Option<u32>,
    /// Apply the candidate diff to the working tree on pass.
    /// Defaults to false (propose only) — never auto-applies unless explicitly set true.
    #[serde(default)]
    pub apply: Option<bool>,
    /// Keep the worktree after the run. Artifacts are always kept.
    #[serde(default)]
    pub keep_worktree: Option<bool>,
    /// Model to build with: a name from builder.models, or a raw provider/model id.
    #[serde(default)]
    pub model: Option<String>,
    /// Fallback models to try if the selected model errors or stalls.
    #[serde(default)]
    pub fallback_models: Option<Vec<String>>,
    /// Override verify gate commands for this run.
    #[serde(default)]
    pub verify_cmds: Option<Vec<String>>,
    /// Restrict this run to paths with these prefixes.
    #[serde(default)]
    pub allow_paths: Option<Vec<String>>,
    /// Override max changed files for this run.
    #[serde(default)]
    pub max_changed_files: Option<usize>,
    /// Override max changed lines for this run.
    #[serde(default)]
    pub max_changed_lines: Option<usize>,
    /// Judge behavior after verify passes: advisory, blocking, retry_on_fail.
    #[serde(default)]
    pub judge_policy: Option<String>,
    /// Tier: cheap | large | frontier. Overrides bob.yaml default_tier.
    #[serde(default)]
    pub tier: Option<String>,
    /// Try only the selected `model`: no tier escalation, no fallback models.
    #[serde(default)]
    pub skip_escalation: Option<bool>,
    /// Name this run so its events path (<artifacts.dir>/<run_id>/events.jsonl)
    /// is known before spawn. Must be fresh (no existing run dir) and
    /// filesystem/git-ref-safe. Omit to auto-mint an id.
    #[serde(default)]
    pub run_id: Option<String>,
}

#[tool_router]
impl BobServer {
    #[tool(description = "Run the build-verify-judge loop on a task/spec; \
returns a RunResult JSON with fields: status, base_sha, iterations, applied, \
next_action, verify, judge, scope, changed_files, stop_reason, final_diff. \
apply defaults to false (propose only); fallback_models retries builder errors/stalls. \
run_id optionally names the run so its events path is known before spawn — must be \
fresh and filesystem/git-ref-safe, or the tool errors.")]
    pub async fn build(&self, Parameters(p): Parameters<BuildParams>) -> String {
        json_or_error(run_build(p).await)
    }
}

impl BobServer {
    pub fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }
}

async fn run_build(p: BuildParams) -> anyhow::Result<String> {
    let mut cfg = Config::load(None)?;
    if let Some(m) = p.max_iters {
        cfg.loop_cfg.max_iterations = m;
    }
    if let Some(cmds) = p.verify_cmds {
        cfg.verify.cmds = cmds;
    }
    if let Some(paths) = p.allow_paths.clone() {
        cfg.scope.allow_paths = paths;
    }
    let allow_paths_for_opts = p.allow_paths.unwrap_or_default();
    if let Some(n) = p.max_changed_files {
        cfg.scope.max_changed_files = n;
    }
    if let Some(n) = p.max_changed_lines {
        cfg.scope.max_changed_lines = n;
    }
    if let Some(policy) = p.judge_policy {
        cfg.judge.policy = policy.parse().map_err(|e: String| anyhow::anyhow!(e))?;
    }
    let spec = p.spec.unwrap_or_else(|| p.task.clone());
    let apply = p.apply.unwrap_or(false); // safety: never auto-apply over MCP without opt-in
    let files = p
        .files
        .unwrap_or_default()
        .into_iter()
        .map(std::path::PathBuf::from)
        .collect();
    let fallback_models = p.fallback_models.unwrap_or_default();
    let run_id = match p.run_id {
        Some(id) => {
            engine::validate_run_id(&id).map_err(|e| anyhow::anyhow!(e))?;
            engine::check_run_id_collision(&cfg.artifacts.dir, &id).map_err(|e| anyhow::anyhow!(e))?;
            id
        }
        None => format!(
            "mcp-{}-{}",
            std::process::id(),
            MCP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ),
    };
    let opts = engine::RunOpts {
        spec,
        context_files: files,
        apply,
        keep_worktree: p.keep_worktree.unwrap_or(false),
        editable_paths: allow_paths_for_opts,
        run_id,
        builder_model: None,
        tier: p.tier,
    };
    let skip_escalation = p.skip_escalation.unwrap_or(false);
    let res = engine::run_opencode_with_fallbacks(&cfg, opts, p.model, fallback_models, skip_escalation)
        .await?;
    Ok(report::to_json(&res))
}

#[tool_handler]
impl ServerHandler for BobServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "bob: autonomous build-verify-judge loop over opencode + abe.".into(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}

fn json_or_error(r: anyhow::Result<String>) -> String {
    match r {
        Ok(s) => s,
        Err(e) => format!(
            "{{\"error\":{}}}",
            serde_json::to_string(&e.to_string())
                .unwrap_or_else(|_| "{\"error\":\"internal serialization error\"}".to_string())
        ),
    }
}

/// Run the MCP server over stdio until shutdown.
pub async fn serve() -> anyhow::Result<()> {
    let server = BobServer::new();
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
