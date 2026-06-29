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

fn gitignore_ignores_bob(dir: &std::path::Path) -> bool {
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

fn append_to_gitignore(dir: &std::path::Path, lines: &[&str]) -> std::io::Result<()> {
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

pub fn run() -> anyhow::Result<()> {
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
                println!("[ok] goose (available; not used by current tier config)");
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
}
