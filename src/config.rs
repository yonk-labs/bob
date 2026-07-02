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
    /// Named roster of models the builder can use. Each entry is EITHER the
    /// legacy `name: "provider/model"` string, OR the explicit form (matching
    /// hector/abe) `name: { model, base_url, api_key_env }` — the latter lets the
    /// thin/goose builders reach an endpoint without the hardcoded prefix guess.
    #[serde(default)]
    pub models: BTreeMap<String, ModelDef>,
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
    /// Bias the within-tier ranking: 0.0 = pure speed, 0.5 = balanced (default,
    /// = reliability × speed), 1.0 = pure reliability. Lets you say "I'd rather
    /// wait for a model that succeeds" or "give me the fastest, flakiness aside".
    #[serde(default = "default_reliability_weight")]
    pub reliability_weight: f64,
    /// Models (roster aliases or raw ids) to try FIRST, in this order, ahead of
    /// stats ranking. A hard "always start here" override.
    #[serde(default)]
    pub pin: Vec<String>,
    /// Models (roster aliases or raw ids) to NEVER attempt — dropped from every
    /// tier chain. A hard "don't use this" override.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Set GOOSE_TOOLSHIM=true for the goose builder — makes goose interpret tool
    /// calls out of plain-text model output. Needed when the serving stack can't
    /// return structured `tool_calls` (parser-less Ollama, custom templates);
    /// otherwise goose makes no edits and bob reports EmptyDiffAfterCritique.
    #[serde(default)]
    pub goose_toolshim: bool,
}

fn default_reliability_weight() -> f64 {
    0.5
}
/// A roster entry: either a bare `provider/model` id (legacy) or the explicit
/// shape shared with hector/abe. Untagged so both YAML forms just work.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum ModelDef {
    /// `qwen: "ollama/Intel/Qwen3-..."` — the opencode-style provider/model id.
    Id(String),
    /// `qwen: { model: "Intel/Qwen3-...", base_url: "http://...:8000/v1" }`.
    Full {
        model: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base_url: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        api_key_env: Option<String>,
    },
}

impl ModelDef {
    /// The model id used for selection/display (and `--model` for opencode).
    pub fn id(&self) -> &str {
        match self {
            ModelDef::Id(s) => s,
            ModelDef::Full { model, .. } => model,
        }
    }
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

    /// True if any tier has at least one model. When false, bob falls back to
    /// the legacy single-attempt opencode path (run `builder.cmd` with its
    /// default/overridden model) instead of the tier escalation chain.
    pub fn any_configured(&self) -> bool {
        !(self.cheap.is_empty()
            && self.medium.is_empty()
            && self.large.is_empty()
            && self.frontier.is_empty())
    }

    /// True if any configured (non-empty) tier resolves to the goose builder.
    /// `bob doctor` uses this to require goose only when the config needs it.
    pub fn uses_goose(&self) -> bool {
        ["cheap", "medium", "large", "frontier"]
            .iter()
            .any(|t| self.builder_for(t) == "goose" && !self.models_for(t).is_empty())
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
                .map(|d| d.id().to_string())
                .unwrap_or_else(|| sel.to_string()),
        )
    }

    /// True if `alias` (or the concrete id it resolves to) is in the `exclude`
    /// list. Matches whether the user listed the roster alias or the raw id.
    pub fn is_excluded(&self, alias: &str) -> bool {
        let target = self.resolved_model(Some(alias));
        self.exclude
            .iter()
            .any(|e| e == alias || (target.is_some() && self.resolved_model(Some(e)) == target))
    }

    /// The roster entry for a selection (alias or default), if listed.
    fn entry(&self, sel: Option<&str>) -> Option<&ModelDef> {
        let sel = sel.or(self.model.as_deref())?;
        self.models.get(sel)
    }

    /// Explicit endpoint for a listed model (Full form). `None` → caller falls
    /// back to the prefix-derived endpoint (extract_base_url).
    pub fn entry_base_url(&self, sel: Option<&str>) -> Option<String> {
        match self.entry(sel)? {
            ModelDef::Full { base_url, .. } => base_url.clone(),
            ModelDef::Id(_) => None,
        }
    }

    /// Explicit API-key env var for a listed model (Full form).
    pub fn entry_api_key_env(&self, sel: Option<&str>) -> Option<String> {
        match self.entry(sel)? {
            ModelDef::Full { api_key_env, .. } => api_key_env.clone(),
            ModelDef::Id(_) => None,
        }
    }

    /// The bare API model id for a listed Full-form model (no provider prefix to
    /// strip). `None` for the legacy string form → caller uses extract_model_name.
    pub fn entry_api_model(&self, sel: Option<&str>) -> Option<String> {
        match self.entry(sel)? {
            ModelDef::Full { model, .. } => Some(model.clone()),
            ModelDef::Id(_) => None,
        }
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
    fn exclude_matches_alias_or_resolved_id() {
        let yaml = r#"
builder:
  cmd: opencode
  models:
    qwen: ollama/Intel/Qwen3-Coder
  exclude: [qwen]
judge: { cmd: abe }
verify: { cmds: [] }
"#;
        let b = serde_yaml::from_str::<Config>(yaml).unwrap().builder;
        assert!(b.is_excluded("qwen")); // by alias
        assert!(b.is_excluded("ollama/Intel/Qwen3-Coder")); // by resolved id
        assert!(!b.is_excluded("minimax/MiniMax-M3")); // unrelated model
    }

    #[test]
    fn reliability_weight_defaults_to_balanced() {
        let yaml = "builder: { cmd: opencode }\njudge: { cmd: abe }\nverify: { cmds: [] }";
        let b = serde_yaml::from_str::<Config>(yaml).unwrap().builder;
        assert_eq!(b.reliability_weight, 0.5);
        assert!(b.pin.is_empty() && b.exclude.is_empty());
    }

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
    fn explicit_model_form_carries_endpoint() {
        // The shape shared with hector/abe: name -> { model, base_url, api_key_env }.
        let yaml = r#"
builder:
  cmd: opencode
  models:
    qwen: { model: "Intel/Qwen3", base_url: "http://host:8000/v1" }
    cloud: { model: "MiniMax-M3", base_url: "https://api.minimax.io/v1", api_key_env: MINIMAX_API_KEY }
    legacy: ollama/Intel/Qwen3-Coder
judge:
  cmd: abe
"#;
        let b = serde_yaml::from_str::<Config>(yaml).unwrap().builder;
        // resolved_model returns the bare model id for the Full form.
        assert_eq!(b.resolved_model(Some("qwen")).as_deref(), Some("Intel/Qwen3"));
        // explicit endpoint + key are exposed (no prefix guessing needed).
        assert_eq!(b.entry_base_url(Some("qwen")).as_deref(), Some("http://host:8000/v1"));
        assert_eq!(b.entry_api_model(Some("qwen")).as_deref(), Some("Intel/Qwen3"));
        assert_eq!(b.entry_api_key_env(Some("cloud")).as_deref(), Some("MINIMAX_API_KEY"));
        // legacy string form carries no explicit endpoint → caller falls back.
        assert!(b.entry_base_url(Some("legacy")).is_none());
        assert!(b.entry_api_model(Some("legacy")).is_none());
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
    // Starting at cheap → escalate through medium → large → frontier
    assert_eq!(
        cfg.ordered_tiers("cheap"),
        vec!["cheap", "medium", "large", "frontier"]
    );
    // Starting at large → only higher tiers, no cheap/medium
    assert_eq!(cfg.ordered_tiers("large"), vec!["large", "frontier"]);
    // Starting at frontier → just frontier
    assert_eq!(cfg.ordered_tiers("frontier"), vec!["frontier"]);

    assert!(cfg.any_configured());
    assert!(!TierCfg::default().any_configured());

    // medium tier has a model and defaults to the goose builder → goose needed.
    assert!(cfg.uses_goose());
    // No tiers configured → no builder needs goose.
    assert!(!TierCfg::default().uses_goose());
    // A tier with a model but a non-goose builder → goose not needed.
    let thin_only = TierCfg {
        cheap: vec!["qwen".into()],
        default_tier: "cheap".into(),
        ..Default::default()
    };
    assert!(!thin_only.uses_goose());
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
