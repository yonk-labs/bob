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

pub fn run() -> anyhow::Result<()> {
    let checks = [("git", which("git")), ("opencode", which("opencode")), ("abe", which("abe"))];
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
        Ok(_) => println!("[ok] config loads"),
        Err(e) => { println!("[MISSING] config: {e}"); ok = false; }
    }
    if ok { Ok(()) } else { anyhow::bail!("doctor found problems") }
}
