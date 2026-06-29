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
    /// Ordered fallback model names/raw ids to try when the selected model stalls or errors.
    #[deprecated(note = "Use `tiers` and `escalation_policy: tier` instead")]
    #[serde(default)]
    pub fallback_models: Vec<String>,
    /// Tiered model assignment. Models within a tier are tried in order (ranked
    /// by model_stats); when a tier fails entirely, escalation moves to the next tier.
    ///
    /// Default tier used when slice.tier isn't set: `default_tier` (defaults to
    /// "cheap"). Slices can override with `tier: frontier` in campaign YAML.
    #[serde(default)]
    pub tiers: TierCfg,
    /// How to escalate when the current model/tier fails:
    ///   "tier"   — exhaust all models in tier, then move to next tier (recommended)
    ///   "model"  — try each next model in order across all tiers (legacy behavior)
    ///   "none"   — try only the selected model, no escalation
    #[serde(default = "default_escalation_policy")]
    pub escalation_policy: String,
    /// Extra builder flags before the prompt, e.g. ["--variant", "high"].
    #[serde(default)]
    pub args: Vec<String>,
}
fn default_builder_timeout() -> u64 {
    600
}
fn default_escalation_policy() -> String {
    "tier".into()
}

/// Three-tier model configuration: cheap → large → frontier.
/// Within each tier, model_stats ranks models by success_rate × (1/avg_latency).
/// Use aliases (from `models:` map) or raw provider/model ids.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct TierCfg {
    #[serde(default)]
    pub cheap: Vec<String>,
    #[serde(default)]
    pub medium: Vec<String>,
    #[serde(default)]
    pub large: Vec<String>,
    #[serde(default)]
    pub frontier: Vec<String>,
    #[serde(default = "default_default_tier")]
    pub default_tier: String,
    #[serde(default)]
    pub cheap_builder: Option<String>,
    #[serde(default)]
    pub medium_builder: Option<String>,
    #[serde(default)]
    pub large_builder: Option<String>,
    #[serde(default)]
    pub frontier_builder: Option<String>,
}
fn default_default_tier() -> String {
    "cheap".into()
}

impl TierCfg {
    /// Escalation order: cheap(small) → medium → large → frontier(complex)
    pub fn escalation_order(&self) -> Vec<&str> {
        vec!["cheap", "medium", "large", "frontier"]
    }

    /// Models in the given tier, or empty if tier name unknown.
    pub fn models_for(&self, tier: &str) -> &[String] {
        match tier {
            "cheap" => &self.cheap,
            "medium" => &self.medium,
            "large" => &self.large,
            "frontier" => &self.frontier,
            _ => &[],
        }
    }

    /// Which builder backend to use for the given tier.
    /// cheap=thin (single-shot), medium/large=goose (agent loop), frontier=opencode (full)
    pub fn builder_for(&self, tier: &str) -> &str {
        match tier {
            "cheap" => self.cheap_builder.as_deref().unwrap_or("thin"),
            "medium" => self.medium_builder.as_deref().unwrap_or("goose"),
            "large" => self.large_builder.as_deref().unwrap_or("goose"),
            "frontier" => self.frontier_builder.as_deref().unwrap_or("opencode"),
            _ => "opencode",
        }
    }

    /// Build the full ordered list of tiers for escalation. The first entry is
    /// the slice's tier; subsequent entries are higher tiers in escalation
    /// order. Within each tier, the caller ranks by model_stats.score.
    pub fn ordered_tiers(&self, slice_tier: &str) -> Vec<String> {
        let order = self.escalation_order();
        let start_idx = order.iter().position(|t| *t == slice_tier).unwrap_or(0);
        let mut out: Vec<String> = Vec::new();
        for t in &order[start_idx..] {
            if !out.contains(&t.to_string()) {
                out.push(t.to_string());
            }
        }
        // If slice_tier wasn't found in the order, prepend it as a starting point
        if out.is_empty() {
            out.push(slice_tier.to_string());
        }
        out
    }
}

impl BuilderCfg {
    /// Resolve a model selection (CLI/MCP override, else the config `model`) to a
    /// concrete id: a key in `models` maps to its value; anything else is used as a
    /// raw id. `None` => no `--model` flag (opencode uses its own default).
    pub fn resolved_model(&self, override_sel: Option<&str>) -> Option<String> {
        let sel = override_sel.or(self.model.as_deref())?;
        Some(
            self.models
                .get(sel)
                .cloned()
                .unwrap_or_else(|| sel.to_string()),
        )
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

    pub fn model_sequence(
        &self,
        override_sel: Option<&str>,
        override_fallbacks: &[String],
    ) -> Vec<Option<String>> {
        let mut out = vec![override_sel
            .map(str::to_string)
            .or_else(|| self.model.clone())];
        let fallbacks = if override_fallbacks.is_empty() {
            &self.fallback_models
        } else {
            override_fallbacks
        };
        out.extend(fallbacks.iter().cloned().map(Some));
        out.dedup();
        out
    }

    pub fn unresolved_aliases(&self) -> Vec<String> {
        let mut refs = Vec::new();
        if let Some(model) = &self.model {
            refs.push(model);
        }
        refs.extend(self.fallback_models.iter());
        refs.into_iter()
            .filter(|name| !self.models.contains_key(name.as_str()) && !name.contains('/'))
            .cloned()
            .collect()
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JudgeCfg {
    pub cmd: String,
    #[serde(default)]
    pub mode: JudgeMode,
    #[serde(default = "default_judge_timeout")]
    pub timeout_secs: u64,
    #[serde(default)]
    pub policy: JudgePolicy,
}
fn default_judge_timeout() -> u64 {
    600
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Default, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum JudgeMode {
    #[default]
    Validate,
    Debate,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum JudgePolicy {
    #[default]
    Advisory,
    Blocking,
    RetryOnFail,
}

impl JudgePolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            JudgePolicy::Advisory => "advisory",
            JudgePolicy::Blocking => "blocking",
            JudgePolicy::RetryOnFail => "retry_on_fail",
        }
    }
}

impl std::str::FromStr for JudgePolicy {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "advisory" => Ok(JudgePolicy::Advisory),
            "blocking" => Ok(JudgePolicy::Blocking),
            "retry_on_fail" => Ok(JudgePolicy::RetryOnFail),
            _ => Err("expected advisory, blocking, or retry_on_fail".to_string()),
        }
    }
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
    fn default() -> Self {
        Self {
            max_iterations: default_max_iters(),
            max_walltime_secs: default_max_walltime(),
        }
    }
}
fn default_max_iters() -> u32 {
    3
}
fn default_max_walltime() -> u64 {
    1800
}

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
    fn default() -> Self {
        Self {
            max_changed_files: default_max_files(),
            max_changed_lines: default_max_lines(),
            allow_paths: vec![],
        }
    }
}
fn default_max_files() -> usize {
    20
}
fn default_max_lines() -> usize {
    800
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ArtifactsCfg {
    #[serde(default = "default_artifacts_dir")]
    pub dir: String,
}
impl Default for ArtifactsCfg {
    fn default() -> Self {
        Self {
            dir: default_artifacts_dir(),
        }
    }
}
fn default_artifacts_dir() -> String {
    ".bob/runs".to_string()
}

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
        if let Some(p) = explicit {
            return Ok(p.to_path_buf());
        }
        let local = PathBuf::from("bob.yaml");
        if local.exists() {
            return Ok(local);
        }
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
  fallback_models: [m3]
  models:
    qwen: ollama/Intel/Qwen3-Coder
    m3: minimax/MiniMax-M3
  args: ["--variant", "high"]
judge:
  cmd: abe
"#;
        let b = serde_yaml::from_str::<Config>(yaml).unwrap().builder;
        // default `model: qwen` resolves via the roster
        assert_eq!(
            b.resolved_model(None).as_deref(),
            Some("ollama/Intel/Qwen3-Coder")
        );
        // per-run override by name
        assert_eq!(
            b.resolved_model(Some("m3")).as_deref(),
            Some("minimax/MiniMax-M3")
        );
        // override with a raw id not in the roster passes through
        assert_eq!(
            b.resolved_model(Some("foo/bar")).as_deref(),
            Some("foo/bar")
        );
        // opencode args: --model <resolved> then the extra args
        assert_eq!(
            b.opencode_args(None),
            vec!["--model", "ollama/Intel/Qwen3-Coder", "--variant", "high"]
        );
        assert_eq!(
            b.model_sequence(None, &[]),
            vec![Some("qwen".into()), Some("m3".into())]
        );
        assert_eq!(
            b.model_sequence(Some("raw/model"), &["m3".into()]),
            vec![Some("raw/model".into()), Some("m3".into())]
        );
        assert!(b.unresolved_aliases().is_empty());
        let mut b3 = b.clone();
        b3.fallback_models = vec!["typo".into(), "raw/model".into()];
        assert_eq!(b3.unresolved_aliases(), vec!["typo"]);
        // no default + no override => no --model (opencode's own default)
        let b2 = serde_yaml::from_str::<Config>("builder: { cmd: opencode }\njudge: { cmd: abe }")
            .unwrap()
            .builder;
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
        assert_eq!(cfg.judge.policy, JudgePolicy::Advisory);
        assert_eq!(cfg.loop_cfg.max_iterations, 3);
        assert_eq!(cfg.scope.max_changed_files, 20);
        assert!(cfg.verify.cmds.is_empty());
        assert!(!cfg.apply);
    }

    #[test]
    fn parses_full_config() {
        let yaml = r#"
builder: { cmd: opencode, timeout_secs: 900 }
judge: { cmd: abe, mode: debate, policy: retry_on_fail }
verify: { cmds: ["cargo test"] }
loop: { max_iterations: 5, max_walltime_secs: 60 }
scope: { max_changed_files: 2, max_changed_lines: 50, allow_paths: ["src/"] }
apply: true
"#;
        let cfg: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.judge.mode, JudgeMode::Debate);
        assert_eq!(cfg.judge.policy, JudgePolicy::RetryOnFail);
        assert_eq!(cfg.verify.cmds, vec!["cargo test"]);
        assert_eq!(cfg.loop_cfg.max_iterations, 5);
        assert_eq!(cfg.scope.allow_paths, vec!["src/"]);
        assert!(cfg.apply);
    }
}

#[test]
fn tier_models_for_returns_correct_slice() {
    let cfg = TierCfg {
        cheap: vec!["qwen".into(), "gemma".into()],
        large: vec!["llama80b".into()],
        frontier: vec!["codex".into()],
        default_tier: "cheap".into(),
        cheap_builder: None,
        medium: vec![],
        medium_builder: None,
        large_builder: None,
        frontier_builder: None,
    };
    assert_eq!(cfg.models_for("cheap"), &["qwen", "gemma"]);
    assert_eq!(cfg.models_for("large"), &["llama80b"]);
    assert_eq!(cfg.models_for("frontier"), &["codex"]);
    assert!(cfg.models_for("unknown").is_empty());
}

#[test]
fn ordered_tiers_escalates_correctly() {
    let cfg = TierCfg {
        cheap: vec!["qwen".into()],
        large: vec!["llama".into()],
        frontier: vec!["codex".into()],
        default_tier: "cheap".into(),
        cheap_builder: None,
        medium: vec![],
        medium_builder: None,
        large_builder: None,
        frontier_builder: None,
    };
    // Starting at cheap → escalate through large → frontier
    assert_eq!(cfg.ordered_tiers("cheap"), vec!["cheap", "large", "frontier"]);
    // Starting at large → no cheap
    assert_eq!(cfg.ordered_tiers("large"), vec!["large", "frontier"]);
    // Starting at frontier → just frontier
    assert_eq!(cfg.ordered_tiers("frontier"), vec!["frontier"]);
}

#[test]
fn parse_tier_config_from_yaml() {
    let yaml = r#"
cmd: opencode
timeout_secs: 600
tiers:
  cheap: [qwen-193, gemma-133]
  large: [llama-80b]
  frontier: [codex, minimax]
  default_tier: cheap
escalation_policy: tier
"#;
    let cfg: BuilderCfg = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(cfg.tiers.cheap, vec!["qwen-193", "gemma-133"]);
    assert_eq!(cfg.tiers.large, vec!["llama-80b"]);
    assert_eq!(cfg.tiers.frontier, vec!["codex", "minimax"]);
    assert_eq!(cfg.tiers.default_tier, "cheap");
    assert_eq!(cfg.escalation_policy, "tier");
}
