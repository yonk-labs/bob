//! Serial campaign runner for Hector-style Bob slices.
//!
//! Campaigns are stricter than one-off `bob build` runs: multi-slice campaigns
//! require `auto_commit` so every converged slice becomes the next slice's real
//! git base. That keeps longer autonomous runs understandable and recoverable.

use crate::config::{Config, JudgePolicy};
use crate::engine::{self, RunOpts, RunStatus};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Deserialize)]
pub struct Campaign {
    pub name: Option<String>,
    #[serde(default)]
    pub auto_apply: bool,
    #[serde(default)]
    pub auto_commit: bool,
    #[serde(default)]
    pub max_slices: Option<usize>,
    #[serde(default)]
    pub verify_cmds: Vec<String>,
    pub slices: Vec<Slice>,
}

#[derive(Debug, Deserialize)]
pub struct Slice {
    pub name: Option<String>,
    pub task: String,
    #[serde(default)]
    pub spec: Option<String>,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub editable_paths: Vec<String>,
    #[serde(default)]
    pub reference_paths: Vec<String>,
    #[serde(default)]
    pub verify_cmds: Vec<String>,
    #[serde(default)]
    pub allow_paths: Vec<String>,
    #[serde(default)]
    pub max_iters: Option<u32>,
    #[serde(default)]
    pub max_changed_files: Option<usize>,
    #[serde(default)]
    pub max_changed_lines: Option<usize>,
    #[serde(default)]
    pub judge_policy: Option<JudgePolicy>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub fallback_models: Vec<String>,
    /// Tier override for this slice (cheap | medium | large | frontier).
    /// Honored here the same way `hector dispatch` passes `--tier`.
    #[serde(default)]
    pub tier: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CampaignReport {
    pub name: String,
    pub status: String,
    pub result_path: String,
    pub slices: Vec<SliceReport>,
    pub campaign_verify: Option<CampaignVerify>,
}

#[derive(Debug, Serialize)]
pub struct CampaignVerify {
    pub passed: bool,
    pub cmd: Option<String>,
    pub output_tail: String,
}

#[derive(Debug, Serialize)]
pub struct SliceReport {
    pub name: String,
    pub status: String,
    pub applied: bool,
    pub committed: bool,
    pub stop_reason: String,
    pub next_action: String,
    pub changed_files: Vec<String>,
    pub artifact_dir: String,
    pub final_diff: String,
}

pub async fn run_file(path: &Path, base_cfg: &Config) -> anyhow::Result<CampaignReport> {
    let text = std::fs::read_to_string(path)?;
    let campaign: Campaign = serde_yaml::from_str(&text)?;
    run(campaign, base_cfg).await
}

pub async fn run(campaign: Campaign, base_cfg: &Config) -> anyhow::Result<CampaignReport> {
    validate(&campaign)?;
    if campaign.auto_commit {
        // Auto-commit campaigns build a serial history. Starting dirty would
        // make it impossible to tell Bob's changes from the operator's changes.
        require_clean_tree()?;
    }

    let name = campaign.name.clone().unwrap_or_else(|| "campaign".into());
    let result_path = campaign_result_path(&base_cfg.artifacts.dir, &name);
    let result_path_str = result_path.to_string_lossy().to_string();
    let mut reports = Vec::new();
    let mut completed = true;
    let limit = campaign.max_slices.unwrap_or(campaign.slices.len());

    for (idx, slice) in campaign.slices.iter().take(limit).enumerate() {
        let slice_name = slice
            .name
            .clone()
            .unwrap_or_else(|| format!("slice-{}", idx + 1));
        let mut cfg = base_cfg.clone();
        apply_slice_overrides(&mut cfg, slice);

        let run_id = format!(
            "campaign-{}-{}-{}",
            slug(&name),
            idx + 1,
            std::process::id()
        );
        let opts = RunOpts {
            spec: slice_spec(slice),
            context_files: context_files(slice),
            apply: campaign.auto_apply || campaign.auto_commit,
            keep_worktree: false,
            run_id,
            builder_model: None,
            editable_paths: slice.editable_paths.clone(),
            tier: slice.tier.clone(),
        };
        let res = engine::run_opencode_with_fallbacks(
            &cfg,
            opts,
            slice.model.clone(),
            slice.fallback_models.clone(),
            false, // campaigns keep tier escalation
        )
        .await?;

        let mut committed = false;
        if campaign.auto_commit && res.status == RunStatus::Converged && res.applied {
            committed = commit_slice(&res.changed_files, &format!("bob: {}", slice_name))?;
        }
        if res.status != RunStatus::Converged || (campaign.auto_commit && !committed) {
            completed = false;
        }

        let stop_reason = res
            .stop_reason
            .as_ref()
            .map(|r| format!("{r:?}"))
            .unwrap_or_default();
        reports.push(SliceReport {
            name: slice_name,
            status: match res.status {
                RunStatus::Converged => "converged".into(),
                RunStatus::NeedsReview => "needs_review".into(),
                RunStatus::NotConverged => "not_converged".into(),
                RunStatus::Error => "error".into(),
            },
            applied: res.applied,
            committed,
            stop_reason,
            next_action: res.next_action.as_str().into(),
            changed_files: res.changed_files,
            artifact_dir: res.artifact_dir,
            final_diff: res.final_diff,
        });

        if !completed {
            break;
        }
    }

    let mut campaign_verify = None;
    let mut status = if completed { "completed" } else { "stopped" }.to_string();
    if should_run_campaign_verify(&campaign, completed) {
        let cwd = std::env::current_dir()?;
        let vr = crate::verify::run_gates(&campaign.verify_cmds, &cwd);
        if !vr.passed {
            status = "integration_failed".into();
        }
        campaign_verify = Some(CampaignVerify {
            passed: vr.passed,
            cmd: vr.cmd.clone(),
            output_tail: crate::builder::tail(&vr.output, 4000),
        });
    }
    let report = CampaignReport {
        name,
        status,
        result_path: result_path_str,
        slices: reports,
        campaign_verify,
    };
    write_result_artifact(&result_path, &report)?;
    Ok(report)
}

/// The campaign-level integration gate is only meaningful once slices have
/// actually landed in the main tree (auto_apply or auto_commit). In pure
/// propose mode no slice touches the checkout, so running verify_cmds
/// against an unmodified tree would be meaningless and potentially
/// misleading.
fn should_run_campaign_verify(c: &Campaign, completed: bool) -> bool {
    completed && !c.verify_cmds.is_empty() && (c.auto_apply || c.auto_commit)
}

fn validate(c: &Campaign) -> anyhow::Result<()> {
    if c.slices.is_empty() {
        anyhow::bail!("campaign has no slices");
    }
    if c.max_slices == Some(0) {
        anyhow::bail!("campaign max_slices must be > 0");
    }
    let limit = c.max_slices.unwrap_or(c.slices.len()).min(c.slices.len());
    if limit > 1 && !c.auto_commit {
        anyhow::bail!(
            "multi-slice campaigns require auto_commit=true so each slice becomes the next base"
        );
    }
    Ok(())
}

fn apply_slice_overrides(cfg: &mut Config, s: &Slice) {
    if let Some(n) = s.max_iters {
        cfg.loop_cfg.max_iterations = n;
    }
    if !s.verify_cmds.is_empty() {
        cfg.verify.cmds = s.verify_cmds.clone();
    }
    let allow_paths = if s.allow_paths.is_empty() {
        &s.editable_paths
    } else {
        &s.allow_paths
    };
    if !allow_paths.is_empty() {
        cfg.scope.allow_paths = allow_paths.clone();
    }
    if let Some(n) = s.max_changed_files {
        cfg.scope.max_changed_files = n;
    }
    if let Some(n) = s.max_changed_lines {
        cfg.scope.max_changed_lines = n;
    }
    if let Some(policy) = s.judge_policy {
        cfg.judge.policy = policy;
    }
}

fn slice_spec(s: &Slice) -> String {
    match &s.spec {
        Some(spec) => format!("## TASK\n{}\n\n## SPEC\n{spec}", s.task),
        None => s.task.clone(),
    }
}

fn context_files(s: &Slice) -> Vec<PathBuf> {
    // Bob receives both editable and reference paths as context, but the scope
    // guard only permits edits under editable_paths/allow_paths.
    let mut paths = BTreeSet::new();
    paths.extend(s.files.iter().cloned());
    paths.extend(s.editable_paths.iter().cloned());
    paths.extend(s.reference_paths.iter().cloned());
    paths.into_iter().map(PathBuf::from).collect()
}

fn git(args: &[&str]) -> anyhow::Result<String> {
    let out = Command::new("git").args(args).output()?;
    if !out.status.success() {
        anyhow::bail!(
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn require_clean_tree() -> anyhow::Result<()> {
    let status = git(&["status", "--porcelain"])?;
    if !status.trim().is_empty() {
        anyhow::bail!("auto_commit campaigns require a clean working tree");
    }
    Ok(())
}

fn commit_slice(paths: &[String], msg: &str) -> anyhow::Result<bool> {
    if paths.is_empty() {
        return Ok(false);
    }
    let mut add = Command::new("git");
    add.arg("add").arg("-A").arg("--").args(paths);
    let out = add.output()?;
    if !out.status.success() {
        anyhow::bail!("git add failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    let staged = Command::new("git")
        .args(["diff", "--cached", "--quiet", "--exit-code"])
        .status()?;
    if staged.success() {
        return Ok(false);
    }
    git(&["commit", "-m", msg])?;
    Ok(true)
}

fn slug(s: &str) -> String {
    let out = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    out.trim_matches('-').chars().take(40).collect()
}

pub fn to_json(report: &CampaignReport) -> String {
    serde_json::to_string(report).unwrap_or_else(|_| "{\"status\":\"error\"}".into())
}

fn campaign_result_path(artifacts_dir: &str, name: &str) -> PathBuf {
    Path::new(artifacts_dir).join(format!(
        "campaign-{}-{}-result.json",
        slug(name),
        std::process::id()
    ))
}

fn write_result_artifact(path: &Path, report: &CampaignReport) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, to_json(report))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multi_slice_requires_auto_commit() {
        let c = Campaign {
            name: Some("x".into()),
            auto_apply: true,
            auto_commit: false,
            max_slices: None,
            verify_cmds: vec![],
            slices: vec![slice("a"), slice("b")],
        };
        assert!(validate(&c).is_err());
    }

    #[test]
    fn writes_campaign_result_json() {
        let tmp = std::env::temp_dir().join(format!("bob-campaign-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let path = tmp.join("result.json");
        let report = CampaignReport {
            name: "c".into(),
            status: "completed".into(),
            result_path: path.to_string_lossy().to_string(),
            slices: vec![],
            campaign_verify: None,
        };

        write_result_artifact(&path, &report).unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains("\"status\":\"completed\""));
        assert!(saved.contains("\"result_path\""));
    }

    #[test]
    fn editable_paths_become_context_and_allowlist_default() {
        let s = Slice {
            editable_paths: vec!["src/".into()],
            reference_paths: vec!["tests/x.test.js".into()],
            ..slice("a")
        };
        let files = context_files(&s);
        assert!(files.contains(&PathBuf::from("src/")));
        assert!(files.contains(&PathBuf::from("tests/x.test.js")));

        let mut cfg: Config =
            serde_yaml::from_str("builder: { cmd: opencode }\njudge: { cmd: abe }\n").unwrap();
        apply_slice_overrides(&mut cfg, &s);
        assert_eq!(cfg.scope.allow_paths, vec!["src/"]);
    }

    #[test]
    fn spec_includes_task_and_spec_body() {
        let s = Slice {
            spec: Some("must return JSON".into()),
            ..slice("implement endpoint")
        };
        let spec = slice_spec(&s);
        assert!(spec.contains("implement endpoint"));
        assert!(spec.contains("must return JSON"));
    }

    /// CROSS-REPO CONTRACT: this is the campaign shape hector emits
    /// (hector/src/schema.rs). Every field here must keep parsing — a rename in
    /// bob's Slice silently drops the hector-supplied value (serde ignores
    /// unknowns). If this fails, the field name diverged from hector; fix both.
    /// The same YAML is asserted by hector's schema::parses_full_contract.
    #[test]
    fn campaign_field_name_contract() {
        let yaml = r#"
name: c
auto_commit: true
slices:
  - name: s
    task: do x
    spec: the spec
    verify_cmds: [cargo test x]
    editable_paths: [src/x.rs]
    reference_paths: [tests/x_test.rs]
    judge_policy: blocking
    max_iters: 3
    max_changed_files: 5
    max_changed_lines: 50
    tier: medium
"#;
        let c: Campaign = serde_yaml::from_str(yaml).unwrap();
        assert!(c.auto_commit);
        let s = &c.slices[0];
        assert_eq!(s.task, "do x");
        assert_eq!(s.spec.as_deref(), Some("the spec"));
        assert_eq!(s.verify_cmds, vec!["cargo test x"]);
        assert_eq!(s.editable_paths, vec!["src/x.rs"]);
        assert_eq!(s.reference_paths, vec!["tests/x_test.rs"]);
        assert_eq!(s.max_iters, Some(3));
        assert_eq!(s.max_changed_files, Some(5));
        assert_eq!(s.max_changed_lines, Some(50));
        assert_eq!(s.tier.as_deref(), Some("medium"));
        assert!(matches!(s.judge_policy, Some(JudgePolicy::Blocking)));
    }

    #[test]
    fn campaign_level_verify_cmds_parse() {
        let y = "name: c\nverify_cmds: [\"npm run test:all\"]\nslices:\n  - task: t\n";
        let c: Campaign = serde_yaml::from_str(y).unwrap();
        assert_eq!(c.verify_cmds, vec!["npm run test:all"]);
    }

    #[test]
    fn campaign_verify_only_runs_when_slices_land_in_main_tree() {
        // Propose mode (auto_apply=false, auto_commit=false): no slice ever
        // touches the checkout, so the gate must be skipped even though
        // verify_cmds is set and the campaign completed.
        let propose_mode = Campaign {
            name: Some("x".into()),
            auto_apply: false,
            auto_commit: false,
            max_slices: None,
            verify_cmds: vec!["cargo test".into()],
            slices: vec![slice("a")],
        };
        assert!(!should_run_campaign_verify(&propose_mode, true));

        // auto_commit campaigns land slices in the tree, so a completed run
        // with verify_cmds set should run the gate.
        let auto_commit = Campaign {
            name: Some("x".into()),
            auto_apply: false,
            auto_commit: true,
            max_slices: None,
            verify_cmds: vec!["cargo test".into()],
            slices: vec![slice("a")],
        };
        assert!(should_run_campaign_verify(&auto_commit, true));

        // No verify_cmds configured: nothing to run regardless of mode.
        let no_cmds = Campaign {
            name: Some("x".into()),
            auto_apply: true,
            auto_commit: true,
            max_slices: None,
            verify_cmds: vec![],
            slices: vec![slice("a")],
        };
        assert!(!should_run_campaign_verify(&no_cmds, true));

        // Campaign did not complete: gate must not run even if otherwise eligible.
        let not_completed = Campaign {
            name: Some("x".into()),
            auto_apply: true,
            auto_commit: true,
            max_slices: None,
            verify_cmds: vec!["cargo test".into()],
            slices: vec![slice("a")],
        };
        assert!(!should_run_campaign_verify(&not_completed, false));
    }

    fn slice(task: &str) -> Slice {
        Slice {
            name: None,
            task: task.into(),
            spec: None,
            files: vec![],
            editable_paths: vec![],
            reference_paths: vec![],
            verify_cmds: vec![],
            allow_paths: vec![],
            max_iters: None,
            max_changed_files: None,
            max_changed_lines: None,
            judge_policy: None,
            model: None,
            fallback_models: vec![],
            tier: None,
        }
    }
}
