#![allow(dead_code)] // used by engine in later tasks
/// Cheap secret markers. Returns human-readable findings; empty == clean.
pub fn scan(text: &str) -> Vec<String> {
    let markers: &[(&str, &str)] = &[
        ("AKIA", "AWS access key id"),
        ("sk-", "OpenAI-style secret key"),
        ("ghp_", "GitHub personal access token"),
        ("xoxb-", "Slack bot token"),
        ("-----BEGIN", "private key block"),
    ];
    markers.iter()
        .filter(|(m, _)| text.contains(m))
        .map(|(m, label)| format!("possible {label} (matched '{m}')"))
        .collect()
}

pub fn risky_filename(name: &str) -> bool {
    let lower = name.to_lowercase();
    [".env", ".pem", ".key", "id_rsa", "credentials", "secret"]
        .iter().any(|p| lower.contains(p))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn flags_aws_key() {
        assert!(!scan("token=AKIAIOSFODNN7EXAMPLE").is_empty());
    }
    #[test]
    fn clean_text_is_empty() {
        assert!(scan("just some normal code\nfn main(){}").is_empty());
    }
    #[test]
    fn flags_risky_filenames() {
        assert!(risky_filename(".env"));
        assert!(risky_filename("deploy/id_rsa"));
        assert!(!risky_filename("src/main.rs"));
    }
}
