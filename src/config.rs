use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
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

#[derive(Debug, Clone, Deserialize)]
pub struct BuilderCfg {
    pub cmd: String,
    #[serde(default = "default_builder_timeout")]
    pub timeout_secs: u64,
}
fn default_builder_timeout() -> u64 { 600 }

#[derive(Debug, Clone, Deserialize)]
pub struct JudgeCfg {
    pub cmd: String,
    #[serde(default)]
    pub mode: JudgeMode,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum JudgeMode {
    #[default]
    Validate,
    Debate,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct VerifyCfg {
    #[serde(default)]
    pub cmds: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
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

#[derive(Debug, Clone, Deserialize)]
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

#[derive(Debug, Clone, Deserialize)]
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
