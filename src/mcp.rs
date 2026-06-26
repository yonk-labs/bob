//! MCP stdio server exposing a `build` tool so agents can invoke the
//! build-verify-judge loop inline. Thin wrapper over engine::run.

static MCP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

use crate::{builder, config::Config, engine, judge, report};
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
    /// Model to build with: a name from builder.models, or a raw provider/model id.
    #[serde(default)]
    pub model: Option<String>,
}

#[tool_router]
impl BobServer {
    #[tool(
        description = "Run the build-verify-judge loop on a task/spec; \
returns a RunResult JSON with fields: status, base_sha, iterations, applied, \
stop_reason, final_diff. apply defaults to false (propose only)."
    )]
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
    let spec = p.spec.unwrap_or_else(|| p.task.clone());
    let apply = p.apply.unwrap_or(false); // safety: never auto-apply over MCP without opt-in
    let files = p
        .files
        .unwrap_or_default()
        .into_iter()
        .map(std::path::PathBuf::from)
        .collect();
    let b = builder::Opencode {
        cmd: cfg.builder.cmd.clone(),
        timeout: std::time::Duration::from_secs(cfg.builder.timeout_secs),
        args: cfg.builder.opencode_args(p.model.as_deref()),
    };
    let j = judge::Abe {
        cmd: cfg.judge.cmd.clone(),
        mode: cfg.judge.mode,
        timeout: std::time::Duration::from_secs(cfg.judge.timeout_secs),
    };
    let opts = engine::RunOpts {
        spec,
        context_files: files,
        apply,
        keep: false,
        run_id: format!("mcp-{}-{}", std::process::id(),
            MCP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)),
    };
    let res = engine::run(&cfg, opts, &b, &j).await?;
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
            serde_json::to_string(&e.to_string()).unwrap_or_else(|_| "{\"error\":\"internal serialization error\"}".to_string())
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
