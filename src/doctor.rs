fn which(cmd: &str) -> bool {
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    let paths = std::env::split_paths(&path_var);

    let exts = if cfg!(target_os = "windows") {
        std::env::var("PATHEXT")
            .ok()
            .map(|p| {
                p.split(';')
                    .filter(|s| !s.is_empty())
                    .map(|s| {
                        let stripped = if s.starts_with('.') { &s[1..] } else { s };
                        stripped.to_string()
                    })
                    .collect::<Vec<String>>()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    for dir in paths {
        let path = dir.join(cmd);
        if path.is_file() {
            return true;
        }
        for ext in &exts {
            let mut path_with_ext = path.clone();
            path_with_ext.set_extension(ext);
            if path_with_ext.is_file() {
                return true;
            }
        }
    }
    false
}

fn looks_like_js_repo(dir: &std::path::Path) -> bool {
    dir.join("package.json").is_file()
        || dir.join("jest.config.js").is_file()
        || dir.join("jest.config.cjs").is_file()
        || dir.join("jest.config.mjs").is_file()
}

pub(crate) fn gitignore_ignores_bob(dir: &std::path::Path) -> bool {
    let Ok(text) = std::fs::read_to_string(dir.join(".gitignore")) else {
        return false;
    };
    text.lines().any(|line| {
        let line = line.split('#').next().unwrap_or("").trim();
        let line = line.trim_start_matches('/').trim_end_matches('/');
        line == ".bob" || line.starts_with(".bob/")
    })
}

fn gitignore_ignores_node_modules(dir: &std::path::Path) -> bool {
    let Ok(text) = std::fs::read_to_string(dir.join(".gitignore")) else {
        return false;
    };
    text.lines().any(|line| {
        let line = line.split('#').next().unwrap_or("").trim();
        let line = line.trim_start_matches('/').trim_end_matches('/');
        line == "node_modules" || line.starts_with("node_modules/")
    })
}

pub(crate) fn append_to_gitignore(dir: &std::path::Path, lines: &[&str]) -> std::io::Result<()> {
    let path = dir.join(".gitignore");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let mut new_text = existing.clone();
    if !new_text.ends_with('\n') && !new_text.is_empty() {
        new_text.push('\n');
    }
    let to_add: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|l| {
            !existing
                .lines()
                .any(|e| e.trim() == l.trim())
        })
        .collect();
    for line in to_add {
        new_text.push_str(line);
        new_text.push('\n');
    }
    std::fs::write(&path, new_text)
}

pub fn run(probe: bool) -> anyhow::Result<()> {
    let checks = [
        ("git", which("git")),
        ("opencode", which("opencode")),
        ("abe", which("abe")),
    ];
    let mut ok = true;
    for (name, present) in checks {
        println!("{} {}", if present { "[ok]" } else { "[MISSING]" }, name);
        ok &= present;
        if !present && name == "opencode" {
            println!("       Official: curl -fsSL https://opencode.ai/install | bash");
            println!("       Alternative: npm install -g opencode-ai");
            println!("       Alternative: brew install anomalyco/tap/opencode");
        }
        if !present && name == "abe" {
            println!("       (optional — judge is advisory)");
        }
    }
    match crate::config::Config::load(None) {
        Ok(cfg) => {
            println!("[ok] config loads");
            let unresolved = cfg.builder.unresolved_aliases();
            if !unresolved.is_empty() {
                println!(
                    "[warn] builder model aliases not in builder.models: {}",
                    unresolved.join(", ")
                );
                println!("       Use roster aliases, or raw provider/model ids containing '/'.");
            }
            // goose is the builder for medium/large tiers; required only when a
            // tier is actually configured to use it.
            let goose_present = which("goose");
            if cfg.builder.tiers.uses_goose() {
                println!(
                    "{} goose (builder for medium/large tier)",
                    if goose_present { "[ok]" } else { "[MISSING]" }
                );
                if !goose_present {
                    println!("       Required by your tier config (medium_builder/large_builder: goose).");
                    println!("       Install: curl -fsSL https://github.com/block/goose/releases/latest/download/install.sh | bash");
                    ok = false;
                }
            } else if goose_present {
                if cfg.builder.cmd == "goose" {
                    println!("[ok] goose (builder.cmd — no tiers configured)");
                } else {
                    println!("[ok] goose (available; not used by current tier config)");
                }
            }
            if probe {
                if which("curl") {
                    let targets = probe_targets(&cfg);
                    if !print_probe_results(&run_probes(targets, curl_alive)) {
                        ok = false;
                    }
                } else {
                    println!("[fail] curl not found on PATH — cannot probe endpoints");
                    ok = false;
                }
            }
        }
        Err(e) => {
            println!("[MISSING] config: {e}");
            ok = false;
        }
    }
    let cwd = std::env::current_dir()?;
    if looks_like_js_repo(&cwd) && !gitignore_ignores_bob(&cwd) {
        println!("[warn] JS/Jest repo detected, but .gitignore does not ignore .bob/");
        println!("       Add `/.bob` to avoid test runner/module-map collisions.");
        println!("       Also exclude `.bob/**` in your test runner config (e.g. vitest");
        println!("       `--exclude '.bob/**'` or `exclude: ['.bob/**']` in vitest.config) —");
        println!("       .gitignore alone won't stop it from picking up worktree copies of the suite.");
    }
    if looks_like_js_repo(&cwd) && !gitignore_ignores_node_modules(&cwd) {
        println!("[warn] package.json present, but .gitignore does not ignore node_modules/");
        println!("       `npm install` will create hundreds of files; bob's scope check will reject them.");
        if std::env::var("BOB_DOCTOR_FIX").is_ok() {
            if let Err(e) = append_to_gitignore(&cwd, &["node_modules/", "package-lock.json"]) {
                println!("[error] failed to update .gitignore: {e}");
            } else {
                println!("[ok] appended node_modules/ and package-lock.json to .gitignore");
            }
        } else {
            println!("       Set BOB_DOCTOR_FIX=1 to auto-append, or add `node_modules/` manually.");
        }
    }
    if ok {
        Ok(())
    } else {
        anyhow::bail!("doctor found problems")
    }
}

/// One distinct endpoint discovered in the resolved config: every model name
/// that carries an explicit `base_url` (the `Full` roster form) pointing at
/// it, plus which tiers (if any) reference those names. Bare `provider/model`
/// ids with no explicit base_url are never included here — bob refuses to
/// guess their endpoint elsewhere (see engine::extract_base_url), so there's
/// nothing safe to curl.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProbeTarget {
    base_url: String,
    models: Vec<String>,
    tiers: Vec<String>,
}

impl ProbeTarget {
    fn describe(&self) -> String {
        let models = self.models.join(", ");
        if self.tiers.is_empty() {
            format!("models: {models}")
        } else {
            format!("models: {models}; tiers: {}", self.tiers.join(", "))
        }
    }
}

/// Walk `builder.models`, grouping every entry with an explicit base_url by
/// that base_url so a shared endpoint is probed once, and note which tiers
/// (cheap/medium/large/frontier) reference each model name.
fn probe_targets(cfg: &crate::config::Config) -> Vec<ProbeTarget> {
    let mut by_url: std::collections::BTreeMap<String, std::collections::BTreeSet<String>> =
        std::collections::BTreeMap::new();
    for name in cfg.builder.models.keys() {
        if let Some(url) = cfg.builder.entry_base_url(Some(name)) {
            by_url.entry(url).or_default().insert(name.clone());
        }
    }
    by_url
        .into_iter()
        .map(|(base_url, model_set)| {
            let models: Vec<String> = model_set.into_iter().collect();
            let tiers: Vec<String> = ["cheap", "medium", "large", "frontier"]
                .iter()
                .filter(|t| {
                    cfg.builder
                        .tiers
                        .models_for(t)
                        .iter()
                        .any(|m| models.contains(m))
                })
                .map(|t| t.to_string())
                .collect();
            ProbeTarget { base_url, models, tiers }
        })
        .collect()
}

/// Outcome of probing one endpoint.
struct ProbeResult {
    target: ProbeTarget,
    alive: bool,
}

/// Probe every target through `probe_fn` — a thin seam so the grouping/report
/// logic is unit-testable without ever shelling out to curl.
fn run_probes(targets: Vec<ProbeTarget>, probe_fn: impl Fn(&str) -> bool) -> Vec<ProbeResult> {
    targets
        .into_iter()
        .map(|target| {
            let alive = probe_fn(&target.base_url);
            ProbeResult { target, alive }
        })
        .collect()
}

/// The actual network check: `curl -sf -m 3 <base_url>/models`. Exit 0 (curl
/// `-f` treats HTTP >=400 as failure) means alive.
fn curl_alive(base_url: &str) -> bool {
    std::process::Command::new("curl")
        .args([
            "-sf",
            "-m",
            "3",
            "-o",
            "/dev/null",
            &format!("{}/models", base_url.trim_end_matches('/')),
        ])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Print one line per probed endpoint. Returns false if any endpoint is dead
/// (folded into doctor's overall exit status).
fn print_probe_results(results: &[ProbeResult]) -> bool {
    if results.is_empty() {
        println!("[probe] no endpoints with an explicit base_url to probe (builder.models is empty or all entries are raw ids)");
        return true;
    }
    let mut ok = true;
    for r in results {
        if r.alive {
            println!("[ok] endpoint {} alive ({})", r.target.base_url, r.target.describe());
        } else {
            println!(
                "[DEAD] endpoint {} unreachable ({})",
                r.target.base_url,
                r.target.describe()
            );
            ok = false;
        }
    }
    ok
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> std::path::PathBuf {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("bob-doctor-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn detects_js_repo() {
        let dir = tmp();
        assert!(!looks_like_js_repo(&dir));
        std::fs::write(dir.join("package.json"), "{}").unwrap();
        assert!(looks_like_js_repo(&dir));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn detects_bob_gitignore_entry() {
        let dir = tmp();
        assert!(!gitignore_ignores_bob(&dir));
        std::fs::write(dir.join(".gitignore"), "/target\n/.bob\n").unwrap();
        assert!(gitignore_ignores_bob(&dir));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn detects_node_modules_gitignore_entry() {
        let dir = tmp();
        assert!(!gitignore_ignores_node_modules(&dir));
        std::fs::write(dir.join(".gitignore"), "node_modules/\n").unwrap();
        assert!(gitignore_ignores_node_modules(&dir));
        std::fs::write(dir.join(".gitignore"), "/node_modules\n").unwrap();
        assert!(gitignore_ignores_node_modules(&dir));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn appends_missing_gitignore_lines_without_duplicates() {
        let dir = tmp();
        std::fs::write(dir.join(".gitignore"), "/target\n").unwrap();
        append_to_gitignore(&dir, &["node_modules/", "package-lock.json"]).unwrap();
        let text = std::fs::read_to_string(dir.join(".gitignore")).unwrap();
        assert!(text.contains("node_modules/"));
        assert!(text.contains("package-lock.json"));
        assert!(text.contains("/target"));
        // Re-append — should not duplicate
        append_to_gitignore(&dir, &["node_modules/", "package-lock.json"]).unwrap();
        let text2 = std::fs::read_to_string(dir.join(".gitignore")).unwrap();
        assert_eq!(
            text2.matches("node_modules/").count(),
            1,
            "must not duplicate"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    fn cfg_from_yaml(yaml: &str) -> crate::config::Config {
        serde_yaml::from_str(yaml).unwrap()
    }

    #[test]
    fn probe_targets_dedupes_by_base_url_and_skips_raw_ids() {
        let cfg = cfg_from_yaml(
            r#"
builder:
  cmd: opencode
  models:
    qwen: { model: "Intel/Qwen3", base_url: "http://host:8000/v1" }
    qwen-alt: { model: "Intel/Qwen3-Alt", base_url: "http://host:8000/v1" }
    cloud: { model: "MiniMax-M3", base_url: "https://api.minimax.io/v1" }
    legacy: ollama/Intel/Qwen3-Coder
judge:
  cmd: abe
"#,
        );
        let mut targets = probe_targets(&cfg);
        targets.sort_by(|a, b| a.base_url.cmp(&b.base_url));
        assert_eq!(targets.len(), 2, "same base_url must be deduped into one target");
        let host = targets.iter().find(|t| t.base_url == "http://host:8000/v1").unwrap();
        assert_eq!(host.models, vec!["qwen", "qwen-alt"]);
        let cloud = targets
            .iter()
            .find(|t| t.base_url == "https://api.minimax.io/v1")
            .unwrap();
        assert_eq!(cloud.models, vec!["cloud"]);
        // "legacy" has no explicit base_url — never guessed, never probed.
        assert!(targets.iter().all(|t| !t.models.contains(&"legacy".to_string())));
    }

    #[test]
    fn probe_targets_notes_which_tiers_reference_a_model() {
        let cfg = cfg_from_yaml(
            r#"
builder:
  cmd: opencode
  models:
    qwen: { model: "Intel/Qwen3", base_url: "http://host:8000/v1" }
  tiers:
    cheap: [qwen]
    large: [qwen]
    default_tier: cheap
judge:
  cmd: abe
"#,
        );
        let targets = probe_targets(&cfg);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].tiers, vec!["cheap", "large"]);
    }

    #[test]
    fn probe_targets_empty_when_no_explicit_endpoints() {
        let cfg = cfg_from_yaml("builder: { cmd: opencode }\njudge: { cmd: abe }");
        assert!(probe_targets(&cfg).is_empty());
    }

    #[test]
    fn run_probes_uses_injected_probe_fn_without_touching_network() {
        let targets = vec![
            ProbeTarget {
                base_url: "http://alive:8000/v1".into(),
                models: vec!["a".into()],
                tiers: vec![],
            },
            ProbeTarget {
                base_url: "http://dead:8000/v1".into(),
                models: vec!["b".into()],
                tiers: vec![],
            },
        ];
        let results = run_probes(targets, |url| url.contains("alive"));
        assert!(results.iter().find(|r| r.target.base_url.contains("alive")).unwrap().alive);
        assert!(!results.iter().find(|r| r.target.base_url.contains("dead")).unwrap().alive);
    }

    #[test]
    fn print_probe_results_returns_false_when_any_dead() {
        let all_alive = vec![ProbeResult {
            target: ProbeTarget {
                base_url: "http://ok:8000/v1".into(),
                models: vec!["a".into()],
                tiers: vec![],
            },
            alive: true,
        }];
        assert!(print_probe_results(&all_alive));

        let one_dead = vec![
            ProbeResult {
                target: ProbeTarget {
                    base_url: "http://ok:8000/v1".into(),
                    models: vec!["a".into()],
                    tiers: vec![],
                },
                alive: true,
            },
            ProbeResult {
                target: ProbeTarget {
                    base_url: "http://dead:8000/v1".into(),
                    models: vec!["b".into()],
                    tiers: vec![],
                },
                alive: false,
            },
        ];
        assert!(!print_probe_results(&one_dead));
    }

    #[test]
    fn print_probe_results_ok_when_no_targets() {
        assert!(print_probe_results(&[]));
    }
}
