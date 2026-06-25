use crate::config::ScopeCfg;

pub struct ScopeReport { pub files: usize, pub lines: usize, pub within: bool, pub detail: String }

pub fn check(diff: &str, cfg: &ScopeCfg) -> ScopeReport {
    let files = diff.lines().filter(|l| l.starts_with("+++ b/")).count();
    let lines = diff.lines().filter(|l| (l.starts_with('+') || l.starts_with('-'))
        && !l.starts_with("+++") && !l.starts_with("---")).count();
    let mut within = files <= cfg.max_changed_files && lines <= cfg.max_changed_lines;
    let mut detail = format!("{files} files, {lines} lines");
    if !cfg.allow_paths.is_empty() {
        for l in diff.lines().filter(|l| l.starts_with("+++ b/")) {
            let path = l.trim_start_matches("+++ b/");
            if !cfg.allow_paths.iter().any(|p| path.starts_with(p.as_str())) {
                within = false;
                detail = format!("{detail}; path outside allowlist: {path}");
            }
        }
    }
    ScopeReport { files, lines, within, detail }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn cfg(files: usize, lines: usize, allow: Vec<&str>) -> ScopeCfg {
        ScopeCfg { max_changed_files: files, max_changed_lines: lines,
                   allow_paths: allow.into_iter().map(String::from).collect() }
    }
    const DIFF: &str = "+++ b/src/a.rs\n+added\n-removed\n+++ b/src/b.rs\n+x\n";

    #[test]
    fn within_caps() {
        let r = check(DIFF, &cfg(10, 100, vec![]));
        assert!(r.within); assert_eq!(r.files, 2); assert_eq!(r.lines, 3);
    }
    #[test]
    fn exceeds_file_cap() {
        assert!(!check(DIFF, &cfg(1, 100, vec![])).within);
    }
    #[test]
    fn path_outside_allowlist() {
        assert!(!check(DIFF, &cfg(10, 100, vec!["docs/"])).within);
    }
}
