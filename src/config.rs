use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub builder: BuilderCfg,
    pub judge: JudgeCfg,
    #[serde(default)]
    pub verify: VerifyCfg,
    #[serde(rename = "loop", default)]
    pub loop_cfg: LoopCfg,
    #[serde(default)]
    pub scope: ScopeCfg,
    #[serde(default)]
    pub apply: bool,
    #[serde(default)]
    pub artifacts: ArtifactsCfg,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BuilderCfg {
    pub cmd: String,
    #[serde(default = "default_builder_timeout")]
    pub timeout_secs: u64,
    /// Default model: a name from `models`, or a raw provider/model id.
    #[serde(default)]
    pub model: Option<String>,
    /// Named roster of models the builder can use (name -> provider/model id).
    #[serde(default)]
    pub models: BTreeMap<String, String>,
    /// Extra builder flags before the prompt, e.g. ["--variant", "high"].
    #[serde(default)]
    pub args: Vec<String>,
}
fn default_builder_timeout() -> u64 { 600 }

impl BuilderCfg {
    /// Resolve a model selection (CLI/MCP override, else the config `model`) to a
    /// concrete id: a key in `models` maps to its value; anything else is used as a
    /// raw id. `None` => no `--model` flag (opencode uses its own default).
    pub fn resolved_model(&self, override_sel: Option<&str>) -> Option<String> {
        let sel = override_sel.or(self.model.as_deref())?;
        Some(self.models.get(sel).cloned().unwrap_or_else(|| sel.to_string()))
    }

    /// Args for `opencode run` before the prompt: the resolved `--model` (if any)
    /// followed by `args`.
    pub fn opencode_args(&self, override_sel: Option<&str>) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(id) = self.resolved_model(override_sel) {
            out.push("--model".to_string());
            out.push(id);
        }
        out.extend(self.args.iter().cloned());
        out
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JudgeCfg {
    pub cmd: String,
    #[serde(default)]
    pub mode: JudgeMode,
    #[serde(default = "default_judge_timeout")]
    pub timeout_secs: u64,
}
fn default_judge_timeout() -> u64 { 600 }

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum JudgeMode {
    #[default]
    Validate,
    Debate,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct VerifyCfg {
    #[serde(default)]
    pub cmds: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LoopCfg {
    #[serde(default = "default_max_iters")]
    pub max_iterations: u32,
    #[serde(default = "default_max_walltime")]
    pub max_walltime_secs: u64,
}
impl Default for LoopCfg {
    fn default() -> Self { Self { max_iterations: default_max_iters(), max_walltime_secs: default_max_walltime() } }
}
fn default_max_iters() -> u32 { 3 }
fn default_max_walltime() -> u64 { 1800 }

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ScopeCfg {
    #[serde(default = "default_max_files")]
    pub max_changed_files: usize,
    #[serde(default = "default_max_lines")]
    pub max_changed_lines: usize,
    #[serde(default)]
    pub allow_paths: Vec<String>,
}
impl Default for ScopeCfg {
    fn default() -> Self { Self { max_changed_files: default_max_files(), max_changed_lines: default_max_lines(), allow_paths: vec![] } }
}
fn default_max_files() -> usize { 20 }
fn default_max_lines() -> usize { 800 }

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ArtifactsCfg {
    #[serde(default = "default_artifacts_dir")]
    pub dir: String,
}
impl Default for ArtifactsCfg {
    fn default() -> Self { Self { dir: default_artifacts_dir() } }
}
fn default_artifacts_dir() -> String { ".bob/runs".to_string() }

impl Config {
    /// Load from an explicit path, else ./bob.yaml, else ~/.config/bob/config.yaml.
    pub fn load(explicit: Option<&Path>) -> anyhow::Result<Config> {
        let path = Self::resolve_path(explicit)?;
        let text = std::fs::read_to_string(&path)
            .map_err(|e| anyhow::anyhow!("reading config {}: {e}", path.display()))?;
        let cfg: Config = serde_yaml::from_str(&text)
            .map_err(|e| anyhow::anyhow!("parsing config {}: {e}", path.display()))?;
        Ok(cfg)
    }

    fn resolve_path(explicit: Option<&Path>) -> anyhow::Result<PathBuf> {
        if let Some(p) = explicit { return Ok(p.to_path_buf()); }
        let local = PathBuf::from("bob.yaml");
        if local.exists() { return Ok(local); }
        let home = std::env::var("HOME").map_err(|_| anyhow::anyhow!("HOME not set"))?;
        Ok(PathBuf::from(home).join(".config/bob/config.yaml"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_roster_resolves_name_default_and_override() {
        let yaml = r#"
builder:
  cmd: opencode
  model: qwen
  models:
    qwen: ollama/Intel/Qwen3-Coder
    m3: minimax/MiniMax-M3
  args: ["--variant", "high"]
judge:
  cmd: abe
"#;
        let b = serde_yaml::from_str::<Config>(yaml).unwrap().builder;
        // default `model: qwen` resolves via the roster
        assert_eq!(b.resolved_model(None).as_deref(), Some("ollama/Intel/Qwen3-Coder"));
        // per-run override by name
        assert_eq!(b.resolved_model(Some("m3")).as_deref(), Some("minimax/MiniMax-M3"));
        // override with a raw id not in the roster passes through
        assert_eq!(b.resolved_model(Some("foo/bar")).as_deref(), Some("foo/bar"));
        // opencode args: --model <resolved> then the extra args
        assert_eq!(b.opencode_args(None), vec!["--model", "ollama/Intel/Qwen3-Coder", "--variant", "high"]);
        // no default + no override => no --model (opencode's own default)
        let b2 = serde_yaml::from_str::<Config>("builder: { cmd: opencode }\njudge: { cmd: abe }").unwrap().builder;
        assert!(b2.resolved_model(None).is_none());
        assert!(b2.opencode_args(None).is_empty());
    }

    #[test]
    fn parses_minimal_config_with_defaults() {
        let yaml = r#"
builder:
  cmd: opencode
judge:
  cmd: abe
"#;
        let cfg: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.builder.cmd, "opencode");
        assert_eq!(cfg.builder.timeout_secs, 600);
        assert_eq!(cfg.judge.timeout_secs, 600);
        assert_eq!(cfg.judge.mode, JudgeMode::Validate);
        assert_eq!(cfg.loop_cfg.max_iterations, 3);
        assert_eq!(cfg.scope.max_changed_files, 20);
        assert!(cfg.verify.cmds.is_empty());
        assert!(!cfg.apply);
    }

    #[test]
    fn parses_full_config() {
        let yaml = r#"
builder: { cmd: opencode, timeout_secs: 900 }
judge: { cmd: abe, mode: debate }
verify: { cmds: ["cargo test"] }
loop: { max_iterations: 5, max_walltime_secs: 60 }
scope: { max_changed_files: 2, max_changed_lines: 50, allow_paths: ["src/"] }
apply: true
"#;
        let cfg: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.judge.mode, JudgeMode::Debate);
        assert_eq!(cfg.verify.cmds, vec!["cargo test"]);
        assert_eq!(cfg.loop_cfg.max_iterations, 5);
        assert_eq!(cfg.scope.allow_paths, vec!["src/"]);
        assert!(cfg.apply);
    }
}
