/// Cheap secret markers. Returns human-readable findings; empty == clean.
pub fn scan(text: &str) -> Vec<String> {
    let markers: &[(&str, &str)] = &[
        ("AKIA", "AWS access key id"),
        ("ghp_", "GitHub personal access token"),
        ("xoxb-", "Slack bot token"),
        ("-----BEGIN", "private key block"),
    ];
    let mut hits: Vec<String> = markers
        .iter()
        .filter(|(m, _)| text.contains(m))
        .map(|(m, label)| format!("possible {label} (matched '{m}')"))
        .collect();
    // `sk-` needs more than a substring match: the bare literal appears inside
    // ordinary words ("task-shaping", "risk-based", "disk-space") and tripped
    // the scanner on innocent context files (finding #29). Only flag it on a
    // token boundary AND followed by a real key-body shape.
    if has_openai_key(text) {
        hits.push("possible OpenAI-style secret key (matched 'sk-')".to_string());
    }
    hits
}

/// True when `text` contains an OpenAI-style `sk-` key: the marker sits on a
/// token boundary (start of string, or a non-alphanumeric char immediately
/// before it) and is followed by a long run of key-body characters
/// ([A-Za-z0-9_-], >= 16). This rejects the substring inside hyphenated words
/// (the char before `sk-` there is alphanumeric) and short fragments, while
/// still catching real keys like `sk-AAAA...` and `sk-proj-...`.
fn has_openai_key(text: &str) -> bool {
    const MIN_BODY: usize = 16;
    for (idx, _) in text.match_indices("sk-") {
        // Boundary: no preceding char, or a non-alphanumeric one.
        let on_boundary = text[..idx]
            .chars()
            .next_back()
            .map_or(true, |c| !c.is_alphanumeric());
        if !on_boundary {
            continue;
        }
        // Body shape: a long run of key chars right after "sk-".
        let body = text[idx + 3..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .count();
        if body >= MIN_BODY {
            return true;
        }
    }
    false
}

pub fn risky_filename(name: &str) -> bool {
    let lower = name.to_lowercase();
    [".env", ".pem", ".key", "id_rsa", "credentials", "secret"]
        .iter()
        .any(|p| lower.contains(p))
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
    fn sk_substring_in_words_does_not_trip() {
        // finding #29: ordinary words that merely contain "sk-" must stay clean.
        assert!(scan("task-shaping the spec").is_empty());
        assert!(scan("a risk-based approach").is_empty());
        assert!(scan("disk-space and desk-setup notes").is_empty());
        // "sk-" on a boundary but with no key body is still not a key.
        assert!(scan("sk- is a prefix").is_empty());
        assert!(scan("sk-short").is_empty());
    }

    #[test]
    fn real_shaped_sk_key_trips() {
        // Obviously-fake but real-SHAPED key body must trip the scanner.
        let hits = scan("OPENAI_API_KEY=sk-AAAAAAAAAAAAAAAAAAAAAAAA");
        assert!(!hits.is_empty(), "real-shaped sk- key must be flagged");
        assert!(hits.iter().any(|h| h.contains("OpenAI")));
        // Boundary variants: start-of-string and after whitespace both count.
        assert!(!scan("sk-BBBBBBBBBBBBBBBBBBBB").is_empty());
        assert!(!scan("key: sk-proj-CCCCCCCCCCCCCCCCCCCC").is_empty());
    }
    #[test]
    fn flags_risky_filenames() {
        assert!(risky_filename(".env"));
        assert!(risky_filename("deploy/id_rsa"));
        assert!(!risky_filename("src/main.rs"));
    }
}
