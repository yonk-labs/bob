use std::process::Command;

fn which(cmd: &str) -> bool {
    Command::new(cmd).arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
}

pub fn run() -> anyhow::Result<()> {
    let checks = [("git", which("git")), ("opencode", which("opencode")), ("abe", which("abe"))];
    let mut ok = true;
    for (name, present) in checks {
        println!("{} {}", if present { "[ok]" } else { "[MISSING]" }, name);
        ok &= present;
    }
    match crate::config::Config::load(None) {
        Ok(_) => println!("[ok] config loads"),
        Err(e) => { println!("[MISSING] config: {e}"); ok = false; }
    }
    if ok { Ok(()) } else { anyhow::bail!("doctor found problems") }
}
