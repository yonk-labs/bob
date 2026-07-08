/// Cheap secret markers. Returns human-readable findings; empty == clean.
pub fn scan(text: &str) -> Vec<String> {
    let markers: &[(&str, &str)] = &[
        ("AKIA", "AWS access key id"),
        ("aws_secret_access_key", "AWS secret access key"),
        ("ghp_", "GitHub personal access token"),
        ("xoxb-", "Slack bot token"),
        ("sk_live_", "Stripe live secret key"),
        ("rk_live_", "Stripe restricted key"),
        ("\"private_key_id\"", "GCP service-account key"),
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
    // JWTs get the same shape treatment: "eyJ" alone appears in prose, so
    // require the three dot-separated base64url segments of a real token.
    if has_jwt(text) {
        hits.push("possible JWT (matched 'eyJ…' three-segment token)".to_string());
    }
    hits
}

/// True when `text` contains a JWT shape: two "eyJ"-prefixed base64url
/// segments (header + payload — base64 of `{"`) and a signature segment,
/// dot-separated. Bare "eyJ" inside a word never has the dotted structure.
fn has_jwt(text: &str) -> bool {
    let b64_run = |s: &str| {
        s.chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
            .count()
    };
    text.match_indices("eyJ").any(|(idx, _)| {
        let rest = &text[idx..];
        let a = b64_run(rest);
        if a < 12 {
            return false;
        }
        let Some(after_a) = rest[a..].strip_prefix('.') else {
            return false;
        };
        let b = b64_run(after_a);
        if b < 12 || !after_a.starts_with("eyJ") {
            return false;
        }
        match after_a[b..].strip_prefix('.') {
            Some(sig) => b64_run(sig) >= 16,
            None => false,
        }
    })
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
            .is_none_or(|c| !c.is_alphanumeric());
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

/// An `Authorization: Bearer <token>` line staged in a 0600 tempfile so the
/// secret never rides a curl argv (`ps` on a shared host shows every argument
/// — B20). Pass `curl_args()` (`-H @<path>`; curl reads header lines from a
/// file since 7.55) and hold the guard until the command has run: dropping it
/// deletes the file.
pub struct BearerHeaderFile {
    path: std::path::PathBuf,
}

impl BearerHeaderFile {
    pub fn new(token: &str) -> std::io::Result<Self> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        // Atomic counter, not the clock — same-tick parallel creates collide.
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("bob-hdr-{}-{n}", std::process::id()));
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        writeln!(f, "Authorization: Bearer {token}")?;
        Ok(Self { path })
    }

    /// The curl args that attach the header: `-H @<path>`.
    pub fn curl_args(&self) -> [String; 2] {
        ["-H".to_string(), format!("@{}", self.path.display())]
    }
}

impl Drop for BearerHeaderFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
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
    fn bearer_header_file_is_0600_and_removed_on_drop() {
        use std::os::unix::fs::PermissionsExt;
        let hdr = BearerHeaderFile::new("sk-TESTKEYTESTKEYTESTKEY").unwrap();
        let path = hdr.path.clone();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text, "Authorization: Bearer sk-TESTKEYTESTKEYTESTKEY\n");
        // the key never appears in the argv-bound args, only the file path
        assert!(hdr.curl_args().iter().all(|a| !a.contains("TESTKEY")));
        drop(hdr);
        assert!(!path.exists(), "header file must be deleted on drop");
    }

    #[test]
    fn flags_risky_filenames() {
        assert!(risky_filename(".env"));
        assert!(risky_filename("deploy/id_rsa"));
        assert!(!risky_filename("src/main.rs"));
    }

    #[test]
    fn flags_stripe_gcp_and_aws_secret_markers() {
        assert!(!scan("STRIPE_KEY=sk_live_AAAABBBBCCCCDDDD").is_empty());
        assert!(!scan("rk_live_AAAABBBBCCCCDDDD").is_empty());
        assert!(!scan(r#"{"private_key_id": "abc", "type": "service_account"}"#).is_empty());
        assert!(!scan("aws_secret_access_key = wJalrXUtnFEMI").is_empty());
    }

    #[test]
    fn flags_jwt_shape_but_not_bare_eyj_prose() {
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dBjftJeZ4CVPmB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert!(scan(&format!("Authorization: Bearer {jwt}"))
            .iter()
            .any(|h| h.contains("JWT")));
        assert!(scan("the word eyJustSomeText is fine").is_empty());
        assert!(scan("eyJhbGciOiJIUzI1NiJ9 alone with no dots").is_empty());
    }
}
