//! `bob init` — write a starter `bob.yaml` into the current directory.
//!
//! Non-interactive: drops the bundled example config so the user can edit
//! `builder.cmd` / `judge.cmd` / `verify.cmds` for their project. Refuses to
//! overwrite an existing `bob.yaml`.

const STARTER: &str = include_str!("../config.example.yaml");

pub fn run() -> anyhow::Result<()> {
    let path = std::path::PathBuf::from("bob.yaml");
    if path.exists() {
        anyhow::bail!("bob.yaml already exists here — not overwriting (edit it, or `rm bob.yaml` to regenerate)");
    }
    std::fs::write(&path, STARTER)?;
    println!("Wrote starter config to ./bob.yaml");
    println!("Next: set verify.cmds to this project's test/build command, then run `bob doctor`.");
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn starter_config_is_valid_yaml_and_parses_as_config() {
        // The bundled starter must always parse as a real Config.
        let cfg: crate::config::Config = serde_yaml::from_str(super::STARTER).unwrap();
        assert_eq!(cfg.builder.cmd, "opencode");
        assert_eq!(cfg.judge.cmd, "abe");
        assert!(!cfg.verify.cmds.is_empty());
    }
}
