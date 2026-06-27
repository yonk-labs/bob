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
}
