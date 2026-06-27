use crate::config::ScopeCfg;

#[derive(Debug, Clone)]
pub struct ScopeReport {
    pub files: usize,
    pub lines: usize,
    pub within: bool,
    pub detail: String,
    pub changed_files: Vec<String>,
}

pub fn check(diff: &str, cfg: &ScopeCfg) -> ScopeReport {
    let changed_files = diff
        .lines()
        .filter_map(|l| l.strip_prefix("diff --git a/"))
        .filter_map(|l| l.split_once(" b/").map(|(_, path)| path.to_string()))
        .collect::<Vec<_>>();
    let files = changed_files.len();
    let lines = diff
        .lines()
        .filter(|l| {
            (l.starts_with('+') || l.starts_with('-'))
                && !l.starts_with("+++")
                && !l.starts_with("---")
        })
        .count();
    let mut within = files <= cfg.max_changed_files && lines <= cfg.max_changed_lines;
    let mut detail = format!("{files} files, {lines} lines");
    if !cfg.allow_paths.is_empty() {
        for path in &changed_files {
            if !cfg.allow_paths.iter().any(|p| path.starts_with(p.as_str())) {
                within = false;
                detail = format!("{detail}; path outside allowlist: {path}");
            }
        }
    }
    ScopeReport {
        files,
        lines,
        within,
        detail,
        changed_files,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn cfg(files: usize, lines: usize, allow: Vec<&str>) -> ScopeCfg {
        ScopeCfg {
            max_changed_files: files,
            max_changed_lines: lines,
            allow_paths: allow.into_iter().map(String::from).collect(),
        }
    }
    const DIFF: &str = "diff --git a/src/a.rs b/src/a.rs\n+++ b/src/a.rs\n+added\n-removed\n\
diff --git a/src/b.rs b/src/b.rs\n+++ b/src/b.rs\n+x\n";
    const DELETE_DIFF: &str =
        "diff --git a/src/dead.rs b/src/dead.rs\n--- a/src/dead.rs\n+++ /dev/null\n-old\n";

    #[test]
    fn within_caps() {
        let r = check(DIFF, &cfg(10, 100, vec![]));
        assert!(r.within);
        assert_eq!(r.files, 2);
        assert_eq!(r.lines, 3);
    }
    #[test]
    fn exceeds_file_cap() {
        assert!(!check(DIFF, &cfg(1, 100, vec![])).within);
    }
    #[test]
    fn path_outside_allowlist() {
        assert!(!check(DIFF, &cfg(10, 100, vec!["docs/"])).within);
    }
    #[test]
    fn deleted_file_counts_as_changed_file() {
        let r = check(DELETE_DIFF, &cfg(10, 100, vec!["src/"]));
        assert!(r.within);
        assert_eq!(r.changed_files, vec!["src/dead.rs"]);
    }
}
