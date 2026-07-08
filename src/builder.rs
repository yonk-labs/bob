use std::path::Path;
use std::time::{Duration, Instant};
use tokio::process::Command;

#[derive(Debug, Clone, Default)]
pub struct BuilderOutcome {
    pub stdout_tail: String,
    pub stderr_tail: String,
    pub failure_kind: String,
}

pub trait Builder {
    async fn build(&self, prompt: &str, workdir: &Path) -> anyhow::Result<BuilderOutcome>;
}

/// Dispatch enum — lets engine.rs pick the builder type at runtime without
/// trait objects (async traits aren't object-safe).
pub enum BuilderKind {
    Opencode(Opencode),
    Thin(ThinBuilder),
    Goose(GooseBuilder),
}

impl Builder for BuilderKind {
    async fn build(&self, prompt: &str, workdir: &Path) -> anyhow::Result<BuilderOutcome> {
        match self {
            BuilderKind::Opencode(b) => b.build(prompt, workdir).await,
            BuilderKind::Thin(b) => b.build(prompt, workdir).await,
            BuilderKind::Goose(b) => b.build(prompt, workdir).await,
        }
    }
}

// ── Opencode builder (full agent loop, ~10K+ context floor) ─────────────────

pub struct Opencode {
    pub cmd: String,
    pub timeout: Duration,
    pub args: Vec<String>,
    pub run_id: Option<String>,
}

// (Opencode implementation is further down in this file — unchanged)

// ── Thin builder (curl-based, zero tool schemas, context = task size only) ──

/// Minimal builder that calls an OpenAI-compatible endpoint directly via curl.
/// No agent loop, no tool schemas, no system prompt overhead. The model gets
/// exactly the task content (spec + test + current file + error feedback) —
/// nothing more. Context size is determined by the task, not the harness.
///
/// The model outputs file contents using a simple delimiter format:
///   === src/foo.js ===
///   <file contents>
///   === src/bar.js ===
///   <file contents>
///
/// Or for single-file slices, just the raw file contents.
pub struct ThinBuilder {
    pub model_id: String,
    pub base_url: String,
    pub api_key: Option<String>,
    pub timeout: Duration,
    /// Request completion cap (`max_tokens`). From the model's roster entry
    /// (`models.<name>.max_tokens`) or the 65536 default. Never hardcode a
    /// small cap here: reasoning models think in output tokens first, and a
    /// server like mlx-lm defaults to 512 when the field is omitted.
    pub max_tokens: u32,
    /// build() calls served by this instance (one per loop iteration). Retries
    /// sample hotter — a model stuck at temperature 0.2 repeats itself
    /// byte-for-byte even with the verify critique appended (bench 20260706:
    /// every NoProgress stop was a byte-identical retry diff).
    pub calls: std::sync::atomic::AtomicU32,
}

const THIN_SYSTEM: &str = "\
You are a code editor. The user gives you a task, spec, test, and current file contents.\n\
Two output formats are available; pick per file:\n\
\n\
FORMAT A — targeted edits to an EXISTING file (preferred for modifications,\n\
required when you were shown an excerpt rather than the whole file):\n\
=== EDIT path/to/file.ext ===\n\
<<<<<<< SEARCH\n\
(exact lines copied verbatim from the current file)\n\
=======\n\
(replacement lines)\n\
>>>>>>> REPLACE\n\
You may put several SEARCH/REPLACE hunks under one `=== EDIT … ===` header.\n\
The SEARCH text must match the current file exactly (copy it verbatim,\n\
including indentation) and must be unique in the file — include enough\n\
surrounding lines to pin it down.\n\
\n\
FORMAT B — complete contents for a NEW or fully rewritten small file:\n\
=== path/to/file.ext ===\n\
<complete file contents>\n\
\n\
Rules:\n\
- Output ONLY blocks in the formats above. No markdown fences, no prose.\n\
- You have NO search or tool facility. Everything you can see is already in\n\
  the prompt. If an excerpt looks incomplete, still make your best FORMAT A\n\
  edit now — never reply with narration or requests to look things up.\n\
- After the LAST block, output a line that is exactly `=== END ===`. Anything\n\
  you want to say (notes, concerns, explanations) goes AFTER that line.\n\
- Write in the LANGUAGE the file's extension implies: a .py file gets Python,\n\
  a .js file gets JavaScript, a .rs file gets Rust. Never mix languages.\n\
- NEVER emit FORMAT B for a large existing file you saw only part of —\n\
  that would delete the rest of the file. Use FORMAT A.\n\
- If the task says to fix an error, make the minimal change needed.\n\
- Match the API signature the test expects exactly.\n\
- For JavaScript: use CommonJS (module.exports) unless the existing code uses ESM (export).";

impl Builder for ThinBuilder {
    async fn build(&self, prompt: &str, workdir: &Path) -> anyhow::Result<BuilderOutcome> {
        // The prompt from bob lists context files by NAME (e.g., "- tests/foo.test.js").
        // opencode can read those files itself; the thin builder can't — the model
        // only sees the prompt text. So we read each file and embed its contents
        // inline before sending to the model.
        let enriched_prompt = enrich_with_file_contents(prompt, workdir);

        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));

        // B20: the key goes into a 0600 header file (`-H @file`), never on the
        // argv where `ps` could read it. The guard deletes the file on drop —
        // it must outlive every attempt below.
        let header = match &self.api_key {
            Some(key) => Some(crate::safety::BearerHeaderFile::new(key)?),
            None => None,
        };

        // Param healing: some servers reject request knobs outright
        // (reasoning-class models 400 on any temperature; newer OpenAI models
        // demand max_completion_tokens). Drop/rename per the server's own
        // error text and retry — up to once per healable param.
        let mut params = WireParams {
            max_tokens: self.max_tokens,
            ..WireParams::default()
        };
        // Retry-explore: first call stays at the default; each retry samples
        // hotter. Bench 20260706: +0.15/iter capped 0.65 was too cold to
        // diversify a peaked wrong completion (byte-identical retries at
        // nominal 0.35/0.5); a confident model needs real heat to move.
        let prior_calls = self
            .calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if prior_calls > 0 {
            let t = (0.2 + 0.3 * f64::from(prior_calls)).min(0.9);
            params.temperature = Some(t);
            eprintln!("thin builder: retry {prior_calls} at temperature {t:.2}");
        }
        let content = loop {
            let mut body = serde_json::json!({
                "model": &self.model_id,
                "messages": [
                    {"role": "system", "content": THIN_SYSTEM},
                    {"role": "user", "content": &enriched_prompt},
                ],
            });
            params.apply(&mut body);
            let body_str = serde_json::to_string(&body)?;

            let mut cmd = Command::new("curl");
            cmd.args(thin_curl_args(
                &url,
                self.timeout.as_secs(),
                header.as_ref(),
            ))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .current_dir(workdir);

            let mut child = cmd.spawn()?;
            use tokio::io::AsyncWriteExt;
            if let Some(mut stdin) = child.stdin.take() {
                stdin.write_all(body_str.as_bytes()).await?;
            }
            let output = child.wait_with_output().await?;

            if !output.status.success() {
                anyhow::bail!(
                    "thin builder: curl failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }

            let stdout = String::from_utf8_lossy(&output.stdout);
            let resp: serde_json::Value = serde_json::from_str(&stdout)
                .map_err(|e| anyhow::anyhow!("thin builder: parse response: {e}; {stdout}"))?;

            if let Some(err) = resp.get("error") {
                let msg = err.to_string();
                if heal_wire_params(&msg, &mut params) {
                    eprintln!("bob: endpoint rejected a request param — retrying ({params:?})");
                    continue;
                }
                anyhow::bail!("thin builder: model API error: {err}");
            }

            match resp["choices"][0]["message"]["content"].as_str() {
                Some(c) => break c.to_string(),
                None => {
                    // Reasoning-class models (live-observed: mlx Ornith) fill
                    // `message.reasoning` first; a truncated response ends
                    // (finish_reason "length") before any `content` exists.
                    // Double the cap and retry, bounded, instead of hard-failing.
                    let truncated = resp["choices"][0]["finish_reason"].as_str()
                        == Some("length")
                        || resp["choices"][0]["message"].get("reasoning").is_some();
                    if truncated && params.max_tokens < 131_072 {
                        params.max_tokens = params.max_tokens.saturating_mul(2);
                        eprintln!(
                            "bob: response ended before content (reasoning-model \
                             truncation) — retrying with max_tokens {}",
                            params.max_tokens
                        );
                        continue;
                    }
                    anyhow::bail!("thin builder: no content in response");
                }
            }
        };

        // Parse file blocks from the model's response and write them
        let (files_written, apply_errors) = parse_and_write_files(&content, workdir)?;

        let error_note = if apply_errors.is_empty() {
            String::new()
        } else {
            // Surfaced (not fatal): the diff stays empty and the loop retries;
            // this text is the model-actionable why.
            format!("\nEDIT APPLY ERRORS:\n{}\n", apply_errors.join("\n"))
        };
        Ok(BuilderOutcome {
            stdout_tail: format!(
                "thin builder: wrote {} file(s){}\n{}",
                files_written.len(),
                error_note,
                content.chars().take(2000).collect::<String>()
            ),
            stderr_tail: apply_errors.join("\n"),
            failure_kind: "ok".into(),
        })
    }
}

/// OpenAI-compatible request knobs that some servers reject outright. One
/// shape, healed in place from the server's own error text; every field's
/// "safe" direction is *omission*, so healing can only make requests more
/// conservative.
#[derive(Debug, Clone, Copy)]
struct WireParams {
    temperature: Option<f64>,
    use_completion_tokens: bool,
    max_tokens: u32,
}

impl Default for WireParams {
    fn default() -> Self {
        WireParams {
            temperature: Some(0.2),
            use_completion_tokens: false,
            max_tokens: 65_536,
        }
    }
}

impl WireParams {
    fn apply(&self, body: &mut serde_json::Value) {
        if let Some(t) = self.temperature {
            body["temperature"] = serde_json::json!(t);
        }
        let key = if self.use_completion_tokens {
            "max_completion_tokens"
        } else {
            "max_tokens"
        };
        body[key] = serde_json::json!(self.max_tokens);
    }
}

/// True when the error names a param we can drop/rename (mutating `p` for the
/// retry). Captured vendor wordings: OpenAI "'temperature' does not support
/// 0.7 with this model", Anthropic "`temperature` is deprecated for this
/// model", OpenAI "Use 'max_completion_tokens' instead".
fn heal_wire_params(err: &str, p: &mut WireParams) -> bool {
    let l = err.to_lowercase();
    let rejected = |name: &str| {
        l.contains(name)
            && (l.contains("unsupported")
                || l.contains("deprecated")
                || l.contains("does not support")
                || l.contains("not supported"))
    };
    if p.temperature.is_some() && rejected("temperature") {
        p.temperature = None;
        return true;
    }
    if !p.use_completion_tokens && (rejected("max_tokens") || l.contains("max_completion_tokens")) {
        p.use_completion_tokens = true;
        return true;
    }
    // Cap too large for the serving context window (vLLM: "This model's
    // maximum context length is N tokens. However, you requested ...").
    // Halve and retry; floor keeps a broken endpoint from looping forever.
    if p.max_tokens > 1024
        && (l.contains("maximum context length")
            || (l.contains("max_tokens") && l.contains("too large")))
    {
        p.max_tokens /= 2;
        return true;
    }
    false
}

/// The thin builder's full curl argv (minus the binary): POST `url` with a
/// JSON body from stdin, auth attached via `-H @<header-file>` so no secret
/// ever appears among the args (B20).
fn thin_curl_args(
    url: &str,
    timeout_secs: u64,
    header: Option<&crate::safety::BearerHeaderFile>,
) -> Vec<String> {
    let mut args = vec![
        "-s".to_string(),
        "--max-time".to_string(),
        timeout_secs.to_string(),
        "-X".to_string(),
        "POST".to_string(),
        url.to_string(),
        "-H".to_string(),
        "Content-Type: application/json".to_string(),
    ];
    if let Some(h) = header {
        args.extend(h.curl_args());
    }
    args.push("-d".to_string());
    args.push("@-".to_string());
    args
}

/// Read files mentioned in the "## CONTEXT FILES" section of the prompt and
/// embed their contents inline. Without this, the thin builder's model only
/// sees file NAMES, not contents — it can't implement to a test it can't read.
fn enrich_with_file_contents(prompt: &str, workdir: &Path) -> String {
    let mut enriched = prompt.to_string();

    // Find file paths in the "## CONTEXT FILES" section
    let mut in_context = false;
    let mut file_paths: Vec<String> = Vec::new();

    for line in prompt.lines() {
        if line.starts_with("## CONTEXT FILES") || line.starts_with("## EDITABLE PATHS") {
            in_context = true;
            continue;
        }
        if line.starts_with("## ") {
            in_context = false;
            continue;
        }
        if in_context {
            if let Some(path) = line.trim().strip_prefix("- ") {
                file_paths.push(path.trim().to_string());
            }
        }
    }

    if file_paths.is_empty() {
        return enriched;
    }

    enriched.push_str("\n\n## FILE CONTENTS (read-only reference)\n");
    for path in &file_paths {
        let full = workdir.join(path);
        match std::fs::read_to_string(&full) {
            Ok(contents) => {
                if contents.len() <= THIN_EMBED_WHOLE_CAP {
                    enriched.push_str(&format!("\n--- {path} ---\n{contents}\n"));
                } else {
                    // Big file: verbatim excerpt windows around identifiers the
                    // task/spec mentions — never a silent head-truncation (the
                    // old 4000-char cut fed the model a file's import block and
                    // nothing else; live find, brownfield bench 2026-07-06).
                    let idents = extract_identifiers(prompt);
                    enriched.push_str(&excerpt_windows(path, &contents, &idents));
                }
            }
            Err(_) => {
                enriched.push_str(&format!("\n--- {path} ---\n(file not found)\n"));
            }
        }
    }

    enriched
}

/// Embed cap for a single context file: at or below this, the whole file goes
/// into the thin prompt; above it, excerpt windows. Also used by the engine's
/// context estimator so the budget reflects what is actually sent.
pub const THIN_EMBED_WHOLE_CAP: usize = 24_000;
/// Total bytes of excerpt windows embedded per oversized file.
const THIN_EXCERPT_TOTAL_CAP: usize = 24_000;
/// Lines of context on each side of an excerpt anchor.
const EXCERPT_WINDOW_LINES: usize = 40;

/// Identifier-ish tokens from the task/spec portion of the prompt (everything
/// before the context-file listings), longest first so `import_products_csv`
/// wins over `import`. Boilerplate section words are excluded.
fn extract_identifiers(prompt: &str) -> Vec<String> {
    let head = prompt
        .split("## EDITABLE PATHS")
        .next()
        .unwrap_or(prompt);
    const STOP: &[&str] = &[
        "TASK", "SPEC", "RULES", "CONTEXT", "FILES", "EDITABLE", "PATHS", "tests", "test",
        "file", "files", "fail", "fails", "failed", "pass", "passes", "with", "that", "this",
        "python", "unittest",
    ];
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for tok in head.split(|c: char| !c.is_ascii_alphanumeric() && c != '_') {
        if tok.len() >= 4
            && tok.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && !STOP.contains(&tok)
            && seen.insert(tok.to_string())
        {
            out.push(tok.to_string());
        }
    }
    out.sort_by_key(|t| std::cmp::Reverse(t.len()));
    out
}

/// Verbatim excerpt windows from an oversized file, anchored on definition
/// lines (then first plain occurrences) of the given identifiers. Windows are
/// merged when they overlap and clearly labeled with line ranges so nothing is
/// silently dropped; SEARCH/REPLACE hunks can copy the text exactly.
fn excerpt_windows(path: &str, contents: &str, idents: &[String]) -> String {
    let lines: Vec<&str> = contents.lines().collect();
    let n = lines.len();
    let mut anchors: Vec<usize> = Vec::new();
    let mut push_anchor = |i: usize, anchors: &mut Vec<usize>| {
        if !anchors.contains(&i) {
            anchors.push(i);
        }
    };
    // Pass 1: definition sites (def/class/fn/function <ident>).
    for ident in idents {
        for (i, l) in lines.iter().enumerate() {
            let t = l.trim_start();
            let is_def = ["def ", "class ", "fn ", "function ", "pub fn "]
                .iter()
                .any(|k| t.starts_with(k) && t[k.len()..].starts_with(ident.as_str()));
            if is_def {
                push_anchor(i, &mut anchors);
            }
        }
    }
    // Pass 2: first plain occurrence of each ident not already covered.
    for ident in idents {
        if let Some(i) = lines.iter().position(|l| l.contains(ident.as_str())) {
            if !anchors
                .iter()
                .any(|&a| i >= a.saturating_sub(EXCERPT_WINDOW_LINES) && i <= a + EXCERPT_WINDOW_LINES)
            {
                push_anchor(i, &mut anchors);
            }
        }
    }
    if anchors.is_empty() {
        // No anchor at all: show the head, honestly labeled.
        let head: String = truncate_chars(contents, THIN_EXCERPT_TOTAL_CAP.min(8_000));
        return format!(
            "\n--- {path} (EXCERPT: first lines of {n}; no task identifiers found; \
             use FORMAT A edits, unshown parts are unchanged) ---\n{head}\n"
        );
    }
    anchors.sort_unstable();
    // Merge into ranges.
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for &a in &anchors {
        let lo = a.saturating_sub(EXCERPT_WINDOW_LINES);
        let hi = (a + EXCERPT_WINDOW_LINES).min(n.saturating_sub(1));
        match ranges.last_mut() {
            Some((_, prev_hi)) if lo <= *prev_hi + 1 => *prev_hi = (*prev_hi).max(hi),
            _ => ranges.push((lo, hi)),
        }
    }
    let mut out = format!(
        "\n--- {path} (EXCERPTS of a {n}-line file; unshown parts exist and are \
         unchanged — modify ONLY via FORMAT A search/replace edits) ---\n"
    );
    let mut budget = THIN_EXCERPT_TOTAL_CAP;
    for (lo, hi) in ranges {
        let chunk = lines[lo..=hi].join("\n");
        if chunk.len() + 64 > budget {
            out.push_str("\n(further excerpts omitted for size)\n");
            break;
        }
        budget -= chunk.len() + 64;
        out.push_str(&format!("\n@@ lines {}-{} of {} @@\n{}\n", lo + 1, hi + 1, n, chunk));
    }
    out
}

/// One SEARCH/REPLACE hunk from a `=== EDIT path ===` block.
struct EditHunk {
    search: String,
    replace: String,
}

/// Parse `<<<<<<< SEARCH … ======= … >>>>>>> REPLACE` hunks from an EDIT
/// section body. Tolerates trailing text after the markers.
fn parse_edit_hunks(body: &str) -> Vec<EditHunk> {
    let mut hunks = Vec::new();
    let mut search: Option<String> = None;
    let mut replace: Option<String> = None;
    for line in body.lines() {
        let t = line.trim_end();
        if t.starts_with("<<<<<<<") {
            search = Some(String::new());
            replace = None;
        } else if t.starts_with("=======") && search.is_some() && replace.is_none() {
            replace = Some(String::new());
        } else if t.starts_with(">>>>>>>") {
            if let (Some(s), Some(r)) = (search.take(), replace.take()) {
                hunks.push(EditHunk { search: s, replace: r });
            }
        } else if let Some(r) = replace.as_mut() {
            r.push_str(line);
            r.push('\n');
        } else if let Some(s) = search.as_mut() {
            s.push_str(line);
            s.push('\n');
        }
    }
    hunks
}

/// Apply hunks to a file's contents, all-or-nothing. A hunk's SEARCH text must
/// occur exactly once (after an exact match fails, a trailing-whitespace-
/// lenient line match is tried). Returns the new contents or a precise error
/// the model can act on next iteration.
fn apply_edit_hunks(contents: &str, hunks: &[EditHunk]) -> Result<String, String> {
    let mut cur = contents.to_string();
    for (i, h) in hunks.iter().enumerate() {
        let needle = h.search.trim_end_matches('\n');
        if needle.trim().is_empty() {
            return Err(format!("hunk {}: empty SEARCH block", i + 1));
        }
        let count = cur.matches(needle).count();
        let (start, len) = match count {
            1 => (cur.find(needle).unwrap(), needle.len()),
            0 => match lenient_find(&cur, needle) {
                Some(pair) => pair,
                None => {
                    return Err(format!(
                        "hunk {}: SEARCH text not found in file (copy it verbatim, \
                         including indentation):\n{}",
                        i + 1,
                        truncate_chars(needle, 400)
                    ))
                }
            },
            n => {
                return Err(format!(
                    "hunk {}: SEARCH text matches {n} places — add surrounding lines \
                     to make it unique",
                    i + 1
                ))
            }
        };
        let replacement = h.replace.trim_end_matches('\n');
        cur = format!("{}{}{}", &cur[..start], replacement, &cur[start + len..]);
    }
    Ok(cur)
}

/// Line-based match ignoring trailing whitespace on each line. Returns the
/// byte offset + length of the matched region, only when exactly one match.
fn lenient_find(haystack: &str, needle: &str) -> Option<(usize, usize)> {
    let nlines: Vec<&str> = needle.lines().map(|l| l.trim_end()).collect();
    if nlines.is_empty() {
        return None;
    }
    // Byte offset of each line start.
    let mut starts = vec![0usize];
    for (i, b) in haystack.bytes().enumerate() {
        if b == b'\n' {
            starts.push(i + 1);
        }
    }
    let hlines: Vec<&str> = haystack.lines().collect();
    let mut found: Option<(usize, usize)> = None;
    for w in 0..hlines.len().saturating_sub(nlines.len() - 1) {
        if (0..nlines.len()).all(|k| hlines[w + k].trim_end() == nlines[k]) {
            if found.is_some() {
                return None; // ambiguous
            }
            let start = starts[w];
            let end_line = w + nlines.len() - 1;
            let end = starts[end_line] + hlines[end_line].len();
            found = Some((start, end - start));
        }
    }
    found
}

/// Parse the model's output into files and write them to the workdir.
/// Supports three formats:
/// 1. Edit blocks: "=== EDIT path ===" + SEARCH/REPLACE hunks (existing files)
/// 2. Delimited: "=== path ===\n<contents>\n=== path2 ===\n<contents>"
/// 3. Raw: entire output is a single file (caller must know the path)
///
/// Returns (written paths, apply errors). Apply errors do not abort the build:
/// nothing is written for the failing file, the diff stays empty, and the
/// error text (surfaced via stdout_tail) tells the model what to fix.
fn parse_and_write_files(
    content: &str,
    workdir: &Path,
) -> anyhow::Result<(Vec<String>, Vec<String>)> {
    let mut written = Vec::new();
    let mut apply_errors: Vec<String> = Vec::new();

    // Strip markdown fences if present
    let content = content.trim();
    let content = if content.starts_with("```") {
        let lines: Vec<&str> = content.lines().collect();
        if lines.len() >= 2 {
            lines[1..lines.len() - 1].join("\n")
        } else {
            content.to_string()
        }
    } else {
        content.to_string()
    };

    // Check for delimited format: === path === / === EDIT path ===
    if content.contains("=== ") {
        let mut current_path: Option<String> = None;
        let mut current_is_edit = false;
        let mut current_contents = String::new();

        // Model paths are untrusted: an unsafe one is skipped (logged, not
        // written, not counted) — one bad path must not abort the whole build.
        // A "path" with no dot and no slash is pseudo-tool noise, not a file:
        // an agentic-tuned model emitted `=== SEARCH ===` trying to grep the
        // repo, and the resulting junk file tripped a false ScopeExceeded
        // (brownfield live find). Rejecting it keeps the diff empty so the
        // retry critique can correct the format instead of stopping the run.
        let mut noise_errors: Vec<String> = Vec::new();
        let mut write_file = |path: String, contents: &str, written: &mut Vec<String>| {
            if !path.contains('.') && !path.contains('/') {
                noise_errors.push(format!(
                    "ignored block `=== {path} ===` — not a file path; there is no \
                     search or tool facility, edit the files shown in FILE CONTENTS \
                     using `=== EDIT <path> ===` SEARCH/REPLACE hunks"
                ));
                return anyhow::Ok(());
            }
            match safe_join(workdir, &path) {
                Some(full) => {
                    // Whole-file (FORMAT B) onto a file too big to have been
                    // fully shown = a truncating rewrite: the model only saw an
                    // excerpt, so its "complete" file drops everything unseen
                    // (brownfield live find: a 9,443-line module "rewritten" as
                    // 57 lines). Reject; force FORMAT A. Small/new files: fine.
                    if let Ok(m) = std::fs::metadata(&full) {
                        if m.len() as usize > THIN_EMBED_WHOLE_CAP
                            && strip_wrapping_fence(contents).len() < m.len() as usize / 2
                        {
                            noise_errors.push(format!(
                                "refused whole-file rewrite of {path}: it is {}KB and you \
                                 were shown only an excerpt — editing it this way would \
                                 delete everything you didn't see. Use `=== EDIT {path} ===` \
                                 SEARCH/REPLACE hunks instead.",
                                m.len() / 1024
                            ));
                            return anyhow::Ok(());
                        }
                    }
                    if let Some(parent) = full.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    std::fs::write(&full, strip_wrapping_fence(contents))?;
                    written.push(path);
                }
                None => eprintln!(
                    "thin builder: skipped unsafe model-supplied path {path:?} (absolute or escapes the worktree)"
                ),
            }
            anyhow::Ok(())
        };
        let apply_edits = |path: String,
                           body: &str,
                           written: &mut Vec<String>,
                           errors: &mut Vec<String>| {
            let Some(full) = safe_join(workdir, &path) else {
                eprintln!(
                    "thin builder: skipped unsafe model-supplied path {path:?} (absolute or escapes the worktree)"
                );
                return anyhow::Ok(());
            };
            let hunks = parse_edit_hunks(body);
            if hunks.is_empty() {
                errors.push(format!("EDIT {path}: no SEARCH/REPLACE hunks found"));
                return anyhow::Ok(());
            }
            let existing = match std::fs::read_to_string(&full) {
                Ok(s) => s,
                Err(e) => {
                    errors.push(format!(
                        "EDIT {path}: cannot edit — {e} (use `=== {path} ===` whole-file \
                         format to create a new file)"
                    ));
                    return anyhow::Ok(());
                }
            };
            match apply_edit_hunks(&existing, &hunks) {
                Ok(new_contents) => {
                    std::fs::write(&full, new_contents)?;
                    written.push(path);
                }
                Err(e) => errors.push(format!("EDIT {path}: {e}")),
            }
            anyhow::Ok(())
        };

        for line in content.lines() {
            if let Some(path) = extract_path_delimiter(line) {
                // `=== END ===` terminates file output: the engine prompt asks
                // the model for end-of-response notes, and without a terminator
                // that prose was written INTO the last file (live find: a
                // "### CONCERNS ###" paragraph inside clamp.py = syntax error
                // = every verify iteration fails the same way).
                if path == "END" {
                    break;
                }
                // Flush previous section if any
                if let Some(prev) = current_path.take() {
                    // Route by body, not just header: models routinely emit
                    // SEARCH/REPLACE hunks under a plain `=== path ===` header
                    // (forgetting the EDIT keyword). Honor the intent.
                    if current_is_edit || current_contents.contains("<<<<<<< SEARCH") {
                        apply_edits(prev, &current_contents, &mut written, &mut apply_errors)?;
                    } else {
                        write_file(prev, &current_contents, &mut written)?;
                    }
                    current_contents.clear();
                }
                if let Some(edit_path) = path.strip_prefix("EDIT ") {
                    current_path = Some(edit_path.trim().to_string());
                    current_is_edit = true;
                } else {
                    current_path = Some(path);
                    current_is_edit = false;
                }
            } else if current_path.is_some() {
                current_contents.push_str(line);
                current_contents.push('\n');
            }
        }
        // Flush last section
        if let Some(prev) = current_path {
            if current_is_edit || current_contents.contains("<<<<<<< SEARCH") {
                apply_edits(prev, &current_contents, &mut written, &mut apply_errors)?;
            } else {
                write_file(prev, &current_contents, &mut written)?;
            }
        }
        apply_errors.extend(noise_errors);
    } else {
        // Raw format — can't determine path, write to a default location.
        // This happens when the model ignores the delimiter format.
        // Write to the first editable path if known, otherwise fail.
        // No delimiter blocks at all — usually the model narrating instead of
        // editing ("Let me search for it…"). Writing prose to a made-up file
        // created out-of-scope junk and a false ScopeExceeded (brownfield
        // bench live find); an empty diff + this error retries with critique.
        apply_errors.push(
            "no `=== path ===` or `=== EDIT path ===` blocks found in the reply — \
             output edits in the required format, no prose"
                .into(),
        );
    }

    Ok((written, apply_errors))
}

/// Strip ONE markdown fence wrapping an entire file body. Models wrap
/// whole-file rewrites in ```lang fences even when told not to (live find,
/// v3 overlord A/B: a literal ```python line landed at the top of app.py and
/// the applied file was garbage Python). Conservative: only strips when the
/// FIRST line opens a fence and the LAST line is exactly ``` — fences in the
/// middle of real content are untouched, and a truncated response missing
/// its closing fence is left alone for the verify gate to catch.
fn strip_wrapping_fence(contents: &str) -> String {
    let t = contents.trim();
    let lines: Vec<&str> = t.lines().collect();
    if lines.len() >= 2
        && lines[0].trim_start().starts_with("```")
        && lines[lines.len() - 1].trim() == "```"
    {
        return lines[1..lines.len() - 1].join("\n");
    }
    t.to_string()
}

/// Join a MODEL-SUPPLIED path under `workdir`, rejecting anything that could
/// land outside it: absolute paths (`Path::join` would replace the base) and
/// any `..` component (worktree escape, incl. nested `a/../../b`) — a write
/// outside the worktree is invisible to capture_diff/scope/secret-scan.
/// `./` components are tolerated; at least one normal component is required.
/// ponytail: lexical component scan, no canonicalize — nothing here follows
/// symlinks the model can't also create, and it works for not-yet-existing paths.
fn safe_join(workdir: &Path, path: &str) -> Option<std::path::PathBuf> {
    use std::path::Component;
    let p = Path::new(path);
    let mut has_normal = false;
    for c in p.components() {
        match c {
            Component::Normal(_) => has_normal = true,
            Component::CurDir => {}
            _ => return None, // RootDir / Prefix (absolute) or ParentDir (..)
        }
    }
    has_normal.then(|| workdir.join(p))
}

fn extract_path_delimiter(line: &str) -> Option<String> {
    let line = line.trim();
    if line.starts_with("=== ") && line.ends_with(" ===") {
        let path = &line[4..line.len() - 4];
        if !path.is_empty() {
            return Some(path.to_string());
        }
    }
    None
}

// ── Goose builder (stripped extensions, smaller context floor) ──────────────

/// Adapter for Goose CLI. Goose with stripped extensions lands at ~2-3K context
/// floor (vs opencode's ~10K). Uses Goose's agent loop for multi-step edits
/// but without the massive tool schema overhead.
///
/// Install: `curl -fsSL https://github.com/block/goose/releases/latest/download/install.sh | bash`
/// Configure: strip to single extension (developer) + tiny_model_system.md
/// Derive goose's `OPENAI_HOST` (host only) from a full base URL. goose appends
/// `OPENAI_BASE_PATH` (default `v1/chat/completions`), so the host must NOT carry
/// a trailing `/v1` or slash — otherwise the request path doubles to `/v1/v1/…`.
/// Trims a trailing slash first so a user-written `…/v1/` normalizes the same as
/// `…/v1`. Works for local vLLM (`http://host:8000/v1`) and OpenAI cloud
/// (`https://api.openai.com/v1` → `https://api.openai.com`) alike.
fn openai_host(url: &str) -> &str {
    url.trim_end_matches('/')
        .trim_end_matches("/v1")
        .trim_end_matches('/')
}

pub struct GooseBuilder {
    pub cmd: String,
    pub model: String,
    pub timeout: Duration,
    pub provider: String,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    /// Set GOOSE_TOOLSHIM=true — interpret tool calls from plain-text output when
    /// the endpoint can't return structured tool_calls (see builder.goose_toolshim).
    pub toolshim: bool,
    /// When set, write `.bob/runs/<run_id>/goose.pid` for the reaper — same
    /// contract as Opencode's `opencode.pid`.
    pub run_id: Option<String>,
    /// Idle-stall watchdog threshold (builder.idle_stall_secs). Zero disables.
    /// Kill early when the endpoint shows no running request for this long.
    pub idle_stall: Duration,
}

/// SIGTERM → 200ms grace → SIGKILL, addressed to the PROCESS GROUP (`-pid`).
/// Both builders are setsid'd, so pgid == pid and group signals reach
/// grandchildren — a killed goose must not leave a tool child alive and
/// writing (finding #31's orphan risk). The direct pid is signaled too as a
/// belt-and-suspenders for a child that somehow isn't a group leader.
fn kill_group_with_escalation(pid: u32) {
    let pgid = -(pid as i32);
    unsafe {
        let _ = libc::kill(pgid, libc::SIGTERM);
        let _ = libc::kill(pid as i32, libc::SIGTERM);
    }
    std::thread::sleep(Duration::from_millis(200));
    unsafe {
        let _ = libc::kill(pgid, libc::SIGKILL);
        let _ = libc::kill(pid as i32, libc::SIGKILL);
    }
}

/// Reaper-visible pidfile for a builder child: `.bob/runs/<run_id>/<name>`.
fn builder_pidfile(run_id: &str, name: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(".bob/runs")
        .join(run_id)
        .join(name)
}

/// What the idle-stall watchdog should do at one poll tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleAction {
    /// Endpoint is busy or unobservable — reset the idle timer, keep waiting.
    ResetTimer,
    /// Confirmed idle, but not long enough yet — keep waiting, keep the timer.
    Wait,
    /// Confirmed idle past the threshold — kill the attempt early.
    KillIdle,
}

/// Pure idle-stall decision (F8). Kill ONLY when the endpoint answered with
/// zero running requests (`Some(false)`) continuously for `idle_stall`. A busy
/// endpoint (`Some(true)`) or an unobservable one (`None`, e.g. no /metrics)
/// resets the timer and is NEVER killed — a busy-loop stays governed by the
/// no-progress diff check + wall clock, exactly as the constraint requires.
/// `idle_stall == 0` disables the watchdog.
fn idle_watchdog_decision(
    idle_stall: Duration,
    idle_elapsed: Duration,
    running: Option<bool>,
) -> IdleAction {
    if idle_stall.is_zero() {
        return IdleAction::ResetTimer;
    }
    match running {
        Some(true) | None => IdleAction::ResetTimer,
        Some(false) if idle_elapsed >= idle_stall => IdleAction::KillIdle,
        Some(false) => IdleAction::Wait,
    }
}

impl Builder for GooseBuilder {
    async fn build(&self, prompt: &str, workdir: &Path) -> anyhow::Result<BuilderOutcome> {
        let mut cmd = Command::new(&self.cmd);
        cmd.arg("run")
            .arg("--no-profile")
            .arg("--with-builtin")
            .arg("developer")
            .arg("--quiet")
            .arg("--text")
            .arg(prompt)
            .arg("--model")
            .arg(&self.model)
            .arg("--provider")
            .arg(&self.provider)
            .current_dir(workdir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        // Point goose at the local endpoint. goose's `openai` provider reads
        // OPENAI_HOST (host only, no /v1 — it appends OPENAI_BASE_PATH), NOT
        // OPENAI_BASE_URL. Setting only the latter silently targets api.openai.com,
        // every request fails auth, goose makes no tool calls, and bob reports an
        // empty diff with no error. Set both: HOST for goose, BASE_URL for others.
        if let Some(url) = &self.base_url {
            cmd.env("OPENAI_HOST", openai_host(url));
            cmd.env("OPENAI_BASE_URL", url);
            cmd.env("OPENAI_API_KEY", self.api_key.as_deref().unwrap_or("local"));
        }

        // Interpret tool calls from plain-text output when the server can't return
        // structured tool_calls. Opt-in via builder.goose_toolshim (env still wins
        // if the operator sets GOOSE_TOOLSHIM directly).
        if self.toolshim {
            cmd.env("GOOSE_TOOLSHIM", "true");
        }

        // Point goose's rolling log file at a writable temp dir. We run with
        // --no-profile and pass all config via flags/env, so goose reads nothing
        // from XDG_CONFIG_HOME/HOME — only its logs use the state dir. Without this,
        // goose panics ("failed to create log file") when ~/.local/state is read-only
        // (containers, sandboxed CI), and it keeps log noise out of the worktree.
        cmd.env(
            "XDG_STATE_HOME",
            std::env::temp_dir().join("bob-goose-state"),
        );

        // setsid for process group isolation
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }

        let child = cmd
            .spawn()
            .map_err(|e| anyhow::anyhow!("spawning goose '{}': {e}", self.cmd))?;
        let child_pid = child.id();

        // Pidfile for the reaper (same contract as opencode.pid): if bob dies
        // without cleaning up, reap_orphans can find and kill this goose.
        if let (Some(run_id), Some(pid)) = (&self.run_id, child_pid) {
            let pid_path = builder_pidfile(run_id, "goose.pid");
            if let Some(parent) = pid_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&pid_path, pid.to_string());
        }
        let remove_pidfile = || {
            if let Some(run_id) = &self.run_id {
                let _ = std::fs::remove_file(builder_pidfile(run_id, "goose.pid"));
            }
        };

        // Race the process against the wall-clock deadline AND an idle-stall
        // watchdog (F8). goose is one-shot (`run --text`, stdin null,
        // wait_with_output buffers) so bob can't observe incremental output or
        // poke it — the only bounded response to an idle-wait hang is to kill
        // early and let the fallback wrapper hop. The watchdog polls the
        // endpoint's running-request signal; it never acts while a request is
        // running (busy-loop) or when the signal is unobservable (fail-safe).
        let deadline = Instant::now() + self.timeout;
        let poll = self.idle_poll_period();
        let running_probe = || {
            self.base_url
                .as_deref()
                .and_then(|u| crate::doctor::endpoint_running_request(u, self.api_key.as_deref()))
        };
        let wait = child.wait_with_output();
        tokio::pin!(wait);
        let mut last_active = Instant::now();
        let outcome = loop {
            tokio::select! {
                out = &mut wait => break WaitOutcome::Done(out),
                _ = tokio::time::sleep(poll) => {
                    if Instant::now() >= deadline {
                        break WaitOutcome::WallTimeout;
                    }
                    match idle_watchdog_decision(self.idle_stall, last_active.elapsed(), running_probe()) {
                        IdleAction::ResetTimer => last_active = Instant::now(),
                        IdleAction::Wait => {}
                        IdleAction::KillIdle => break WaitOutcome::IdleStall(last_active.elapsed()),
                    }
                }
            }
        };

        match outcome {
            WaitOutcome::Done(out) => {
                remove_pidfile();
                let out = out?;
                let stdout_tail = tail(&String::from_utf8_lossy(&out.stdout), 4000);
                let stderr_tail = tail(&String::from_utf8_lossy(&out.stderr), 4000);
                if !out.status.success() {
                    anyhow::bail!(
                        "goose exited with status {}; stderr:\n{}",
                        out.status,
                        stderr_tail
                    );
                }
                // goose exits 0 after "Network error: Request timed out — …"
                // against a dead endpoint, with zero tool calls made (repro
                // F2b). Surface that as endpoint_error instead of "ok" so the
                // engine can classify marker + empty diff as an INFRA error
                // and hop models, not burn a judge iteration on nothing.
                let network_err = stdout_tail.contains("Network error:")
                    || stderr_tail.contains("Network error:");
                Ok(BuilderOutcome {
                    stdout_tail,
                    stderr_tail,
                    failure_kind: if network_err {
                        "endpoint_error".into()
                    } else {
                        "ok".into()
                    },
                })
            }
            // Escalated GROUP kill for both terminal cases: kill_on_drop alone
            // SIGKILLs only the direct goose pid, orphaning any tool child.
            WaitOutcome::WallTimeout => {
                if let Some(pid) = child_pid {
                    kill_group_with_escalation(pid);
                }
                remove_pidfile();
                anyhow::bail!("goose timed out after {:?}", self.timeout)
            }
            WaitOutcome::IdleStall(elapsed) => {
                if let Some(pid) = child_pid {
                    kill_group_with_escalation(pid);
                }
                remove_pidfile();
                // "idle-stall" is the classified marker the engine maps to a
                // builder_idle_stall event and the fallback wrapper hops on.
                anyhow::bail!(
                    "goose idle-stalled after {elapsed:?} with no running request on the endpoint — killed early"
                )
            }
        }
    }
}

impl GooseBuilder {
    /// Watchdog poll cadence: frequent enough to notice within a fraction of
    /// the threshold, but never sub-second. Derived from idle_stall (¼ of it,
    /// clamped [2s, 15s]); when disabled, poll rarely — only the wall-clock
    /// deadline matters.
    fn idle_poll_period(&self) -> Duration {
        if self.idle_stall.is_zero() {
            return Duration::from_secs(15);
        }
        let quarter = self.idle_stall / 4;
        quarter.clamp(Duration::from_secs(2), Duration::from_secs(15))
    }
}

/// Terminal outcome of the goose wait/watchdog race.
enum WaitOutcome {
    Done(std::io::Result<std::process::Output>),
    WallTimeout,
    IdleStall(Duration),
}

// ── Opencode builder implementation (unchanged, moved here for cohesion) ────

impl Builder for Opencode {
    async fn build(&self, prompt: &str, workdir: &Path) -> anyhow::Result<BuilderOutcome> {
        let mut cmd = Command::new(&self.cmd);
        cmd.arg("run")
            .arg("--pure") // strip external plugins — reduces system prompt overhead
            .arg("--dir")
            .arg(workdir)
            .args(&self.args)
            .arg(prompt)
            .current_dir(workdir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let child = cmd
            .spawn()
            .map_err(|e| anyhow::anyhow!("spawning builder '{}': {e}", self.cmd))?;
        let child_pid = child.id();

        if let Some(run_id) = &self.run_id {
            let pid_path = std::path::PathBuf::from(".bob/runs")
                .join(run_id)
                .join("opencode.pid");
            if let Some(parent) = pid_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Some(pid) = child_pid {
                let _ = std::fs::write(&pid_path, pid.to_string());
            }
        }

        match tokio::time::timeout(self.timeout, child.wait_with_output()).await {
            Ok(out) => {
                let out = out?;
                let stdout_tail = tail(&String::from_utf8_lossy(&out.stdout), 4000);
                let stderr_tail = tail(&String::from_utf8_lossy(&out.stderr), 4000);
                if let Some(run_id) = &self.run_id {
                    let pid_path = std::path::PathBuf::from(".bob/runs")
                        .join(run_id)
                        .join("opencode.pid");
                    let _ = std::fs::remove_file(&pid_path);
                }
                if !out.status.success() {
                    anyhow::bail!(
                        "builder exited with status {}; stderr tail:\n{}",
                        out.status,
                        stderr_tail
                    );
                }
                Ok(BuilderOutcome {
                    stdout_tail,
                    stderr_tail,
                    failure_kind: "ok".into(),
                })
            }
            Err(_) => {
                // Group kill: opencode is setsid'd too — signaling only the
                // direct pid orphans its grandchildren.
                if let Some(pid) = child_pid {
                    kill_group_with_escalation(pid);
                }
                if let Some(run_id) = &self.run_id {
                    let pid_path = std::path::PathBuf::from(".bob/runs")
                        .join(run_id)
                        .join("opencode.pid");
                    let _ = std::fs::remove_file(&pid_path);
                }
                anyhow::bail!("builder timed out after {:?}", self.timeout)
            }
        }
    }
}

pub fn tail(s: &str, max_chars: usize) -> String {
    let mut chars = s.chars().rev().take(max_chars).collect::<Vec<_>>();
    chars.reverse();
    chars.into_iter().collect()
}

/// First `max_chars` characters, char-boundary safe, with a truncation marker
/// appended if the input was longer. Used to cap context-file embeds without
/// panicking on multibyte UTF-8 (a raw `&s[..n]` byte slice would).
pub fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() > max_chars {
        let head: String = s.chars().take(max_chars).collect();
        format!("{head}...\n(truncated)")
    } else {
        s.to_string()
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_host_strips_v1_for_local_and_cloud() {
        // local vLLM
        assert_eq!(
            openai_host("http://192.168.1.193:8000/v1"),
            "http://192.168.1.193:8000"
        );
        // user-written trailing slash must not double the /v1
        assert_eq!(openai_host("http://host:8000/v1/"), "http://host:8000");
        // OpenAI cloud
        assert_eq!(
            openai_host("https://api.openai.com/v1"),
            "https://api.openai.com"
        );
        // already host-only (no /v1) — unchanged
        assert_eq!(openai_host("http://host:8000"), "http://host:8000");
        assert_eq!(
            openai_host("https://api.openai.com"),
            "https://api.openai.com"
        );
    }

    #[test]
    fn thin_curl_argv_never_contains_key_material() {
        // B20: the api key must reach curl via the 0600 header file, never argv.
        let key = "sk-VERYSECRETVERYSECRETVERYSECRET";
        let hdr = crate::safety::BearerHeaderFile::new(key).unwrap();
        let args = thin_curl_args("http://host:8000/v1/chat/completions", 30, Some(&hdr));
        assert!(
            args.iter().all(|a| !a.contains("VERYSECRET")),
            "key leaked onto curl argv: {args:?}"
        );
        // the header is attached by reference: `-H @<file>`
        assert!(args
            .windows(2)
            .any(|w| w[0] == "-H" && w[1].starts_with('@')));
        // no key → no header-file arg (`-d @-` is the stdin body, not a header)
        let bare = thin_curl_args("http://host:8000/v1/chat/completions", 30, None);
        assert!(!bare
            .windows(2)
            .any(|w| w[0] == "-H" && w[1].starts_with('@')));
    }

    #[test]
    fn parse_delimited_files() {
        let dir = tempdir();
        let content = "\
=== src/foo.js ===
const x = 1;
=== src/bar.js ===
const y = 2;
";
        let (written, _errs) = parse_and_write_files(content, &dir).unwrap();
        assert_eq!(written.len(), 2);
        assert_eq!(written[0], "src/foo.js");
        assert_eq!(written[1], "src/bar.js");
        assert_eq!(
            std::fs::read_to_string(dir.join("src/foo.js")).unwrap(),
            "const x = 1;"
        );
    }

    #[test]
    fn parse_single_delimited_file() {
        let dir = tempdir();
        let content = "\
=== src/body.js ===
class Body { }
";
        let (written, _errs) = parse_and_write_files(content, &dir).unwrap();
        assert_eq!(written, vec!["src/body.js"]);
        assert!(std::fs::read_to_string(dir.join("src/body.js"))
            .unwrap()
            .contains("class Body"));
    }

    #[test]
    fn truncate_chars_is_utf8_safe() {
        // 4001 two-byte chars: a raw &s[..4000] byte slice would panic mid-char.
        let s = "é".repeat(4001);
        let out = truncate_chars(&s, 4000);
        assert!(out.ends_with("(truncated)"));
        assert!(out.starts_with('é'));
        // Short input is returned unchanged, no marker.
        assert_eq!(truncate_chars("hi", 4000), "hi");
    }

    /// I-S1: model-chosen paths are untrusted. An absolute path or any `..`
    /// component must be SKIPPED (never written outside the worktree, never
    /// counted as written), while a normal relative path still lands — the
    /// build must not error out over one rejected file.
    #[test]
    fn parse_rejects_absolute_and_traversal_paths() {
        let dir = tempdir();
        // Escape targets that a vulnerable join would have created.
        let abs_target =
            std::env::temp_dir().join(format!("bob-abs-escape-{}", std::process::id()));
        let rel_target = dir
            .parent()
            .unwrap()
            .join(format!("bob-rel-escape-{}", std::process::id()));
        let _ = std::fs::remove_file(&abs_target);
        let _ = std::fs::remove_file(&rel_target);

        let content = format!(
            "=== {} ===\npwned-abs\n=== ../{} ===\npwned-parent\n=== a/../../{} ===\npwned-nested\n=== src/foo.rs ===\nfn ok() {{}}\n",
            abs_target.display(),
            rel_target.file_name().unwrap().to_string_lossy(),
            rel_target.file_name().unwrap().to_string_lossy(),
        );
        let (written, _errs) = parse_and_write_files(&content, &dir).unwrap();

        assert_eq!(written, vec!["src/foo.rs"], "only the safe path is written");
        assert!(
            !abs_target.exists(),
            "absolute model path escaped the worktree"
        );
        assert!(!rel_target.exists(), "../ model path escaped the worktree");
        assert_eq!(
            std::fs::read_to_string(dir.join("src/foo.rs")).unwrap(),
            "fn ok() {}",
            "safe relative path still writes normally"
        );

        // `./`-prefixed relative paths stay inside the worktree and are fine.
        let (written, _errs) = parse_and_write_files("=== ./src/bar.rs ===\nfn bar() {}\n", &dir).unwrap();
        assert_eq!(written, vec!["./src/bar.rs"]);
        assert!(dir.join("src/bar.rs").exists());
    }

    #[test]
    fn extract_delimiter() {
        assert_eq!(
            extract_path_delimiter("=== src/foo.js ==="),
            Some("src/foo.js".into())
        );
        assert_eq!(extract_path_delimiter("=== not a delimiter"), None);
        assert_eq!(extract_path_delimiter("const x = 1;"), None);
    }

    #[test]
    fn wire_params_heal_from_real_vendor_rejections() {
        let mut p = WireParams::default();
        // Anthropic wording (captured live 2026-07-05).
        assert!(heal_wire_params(
            "`temperature` is deprecated for this model.",
            &mut p
        ));
        assert!(p.temperature.is_none());
        // OpenAI max_tokens wording.
        assert!(heal_wire_params(
            "Unsupported parameter: 'max_tokens' is not supported with this model. \
             Use 'max_completion_tokens' instead.",
            &mut p
        ));
        assert!(p.use_completion_tokens);
        // Fully healed — nothing left to change; unrelated errors never heal.
        assert!(!heal_wire_params("`temperature` is deprecated", &mut p));
        let mut q = WireParams::default();
        assert!(!heal_wire_params("401 Unauthorized", &mut q));
        assert!(!heal_wire_params("model not found", &mut q));
        // apply() writes the right key for each state.
        let mut body = serde_json::json!({});
        WireParams::default().apply(&mut body);
        assert!(body["temperature"].is_number() && body["max_tokens"].is_number());
        let mut body2 = serde_json::json!({});
        p.apply(&mut body2);
        assert!(body2["temperature"].is_null());
        assert!(body2["max_completion_tokens"].is_number() && body2["max_tokens"].is_null());
    }

    #[test]
    fn max_tokens_is_configurable_and_heals_too_large() {
        // The cap comes from config (roster/default), not a hardcoded number.
        let mut p = WireParams {
            max_tokens: 65_536,
            ..WireParams::default()
        };
        let mut body = serde_json::json!({});
        p.apply(&mut body);
        assert_eq!(body["max_tokens"], 65_536);
        // vLLM's too-large rejection halves the cap and retries…
        let err = "This model's maximum context length is 40960 tokens. \
                   However, you requested 70000 tokens";
        assert!(heal_wire_params(err, &mut p));
        assert_eq!(p.max_tokens, 32_768);
        // …down to a floor, then stops (broken endpoint must not loop).
        p.max_tokens = 1024;
        assert!(!heal_wire_params(err, &mut p));
    }

    #[test]
    fn end_delimiter_keeps_trailing_prose_out_of_files() {
        // Live find: the engine prompt invites end-of-response notes; a model's
        // "### CONCERNS ###" paragraph was written INTO the last file (syntax
        // error, every verify iteration failed identically). `=== END ===`
        // terminates capture; the notes never reach disk.
        let dir = tempdir();
        let content = "=== src/foo.py ===\ndef foo():\n    return 1\n=== END ===\n\
                       ### CONCERNS ###\nthis prose must not land in foo.py\n";
        let (written, _errs) = parse_and_write_files(content, &dir).unwrap();
        assert_eq!(written, vec!["src/foo.py"]);
        let body = std::fs::read_to_string(dir.join("src/foo.py")).unwrap();
        assert!(
            !body.contains("CONCERNS"),
            "prose leaked into the file: {body}"
        );
        assert!(body.contains("def foo()"));
        assert!(!dir.join("END").exists(), "END is a terminator, not a file");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn fenced_file_bodies_are_unwrapped() {
        // Live find (v3 overlord A/B, treatment run 2): the model wrapped its
        // whole-file rewrite in ```python fences INSIDE the === path ===
        // delimiters; the fence line was written into app.py and the applied
        // file was garbage Python. Per-file bodies must shed one wrapping fence.
        let dir = tempdir();
        let content = "=== app.py ===\n```python\ndef f():\n    return 1\n```\n=== END ===\n";
        let (written, _errs) = parse_and_write_files(content, &dir).unwrap();
        assert_eq!(written, vec!["app.py"]);
        let body = std::fs::read_to_string(dir.join("app.py")).unwrap();
        assert_eq!(body, "def f():\n    return 1");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn fence_stripping_is_conservative() {
        // Interior fences (real content) survive; a truncated body missing its
        // closing fence is left alone for the verify gate to catch.
        let doc = "# readme\n```sh\nrun me\n```\ntrailing prose";
        assert_eq!(strip_wrapping_fence(doc), doc);
        let truncated = "```python\ndef f():";
        assert_eq!(strip_wrapping_fence(truncated), truncated);
        assert_eq!(strip_wrapping_fence("```python\nx = 1\n```"), "x = 1");
        assert_eq!(strip_wrapping_fence("```\nx = 1\n```"), "x = 1");
    }

    /// Write an executable fake builder script that records its argv (one per
    /// line) to `args.txt` in its cwd, then exits 0.
    fn write_argv_recorder(dir: &Path) -> std::path::PathBuf {
        let script = dir.join("fake-builder.sh");
        std::fs::write(&script, "#!/bin/sh\nprintf '%s\\n' \"$@\" > args.txt\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    /// repro F1: each builder kind must exec with ITS OWN flag set — goose
    /// must never receive opencode's `--pure`/`--dir`, opencode must.
    #[tokio::test]
    async fn opencode_and_goose_compose_their_own_argv() {
        // opencode: run --pure --dir <workdir> ... prompt
        let dir = tempdir();
        let script = write_argv_recorder(&dir);
        let b = Opencode {
            cmd: script.to_string_lossy().into_owned(),
            timeout: Duration::from_secs(5),
            args: vec![],
            run_id: None,
        };
        b.build("the prompt", &dir).await.unwrap();
        let argv = std::fs::read_to_string(dir.join("args.txt")).unwrap();
        let args: Vec<&str> = argv.lines().collect();
        assert_eq!(args[0], "run");
        assert!(args.contains(&"--pure"), "opencode gets --pure: {args:?}");
        assert!(args.contains(&"--dir"), "opencode gets --dir: {args:?}");

        // goose: run --no-profile ... --provider <p>, and NEVER opencode flags
        let dir = tempdir();
        let script = write_argv_recorder(&dir);
        let b = GooseBuilder {
            cmd: script.to_string_lossy().into_owned(),
            model: "m".into(),
            timeout: Duration::from_secs(5),
            provider: "openai".into(),
            base_url: None,
            api_key: None,
            toolshim: false,
            idle_stall: Duration::from_secs(0),
            run_id: None,
        };
        b.build("the prompt", &dir).await.unwrap();
        let argv = std::fs::read_to_string(dir.join("args.txt")).unwrap();
        let args: Vec<&str> = argv.lines().collect();
        assert_eq!(args[0], "run");
        assert!(
            args.contains(&"--no-profile"),
            "goose gets --no-profile: {args:?}"
        );
        assert!(
            args.contains(&"--provider"),
            "goose gets --provider: {args:?}"
        );
        assert!(
            !args.contains(&"--pure"),
            "goose must NOT get opencode's --pure: {args:?}"
        );
        assert!(
            !args.contains(&"--dir"),
            "goose must NOT get opencode's --dir: {args:?}"
        );
    }

    /// repro F2b: goose exits 0 after "Network error: Request timed out" with
    /// zero tokens — that must surface as endpoint_error, never "ok".
    #[tokio::test]
    async fn goose_exit_zero_network_error_is_endpoint_error_not_ok() {
        let dir = tempdir();
        let script = dir.join("fake-goose.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\necho 'Network error: Request timed out — check your network connection and try again.'\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mk = |cmd: String| GooseBuilder {
            cmd,
            model: "m".into(),
            timeout: Duration::from_secs(5),
            provider: "openai".into(),
            base_url: None,
            api_key: None,
            toolshim: false,
            idle_stall: Duration::from_secs(0),
            run_id: None,
        };
        let out = mk(script.to_string_lossy().into_owned())
            .build("p", &dir)
            .await
            .unwrap();
        assert_eq!(out.failure_kind, "endpoint_error");

        // Healthy exit-0 output stays "ok".
        let script_ok = dir.join("fake-goose-ok.sh");
        std::fs::write(&script_ok, "#!/bin/sh\necho 'done editing'\n").unwrap();
        std::fs::set_permissions(&script_ok, std::fs::Permissions::from_mode(0o755)).unwrap();
        let out = mk(script_ok.to_string_lossy().into_owned())
            .build("p", &dir)
            .await
            .unwrap();
        assert_eq!(out.failure_kind, "ok");
    }

    /// F8: the pure idle-stall decision. The two constraints that matter —
    /// never act on a busy endpoint, never act on an unobservable one — are
    /// asserted directly.
    #[test]
    fn idle_watchdog_only_kills_confirmed_idle_past_threshold() {
        let stall = Duration::from_secs(120);
        let long = Duration::from_secs(200);
        let short = Duration::from_secs(30);

        // Confirmed idle (no running request) past the threshold → kill.
        assert_eq!(
            idle_watchdog_decision(stall, long, Some(false)),
            IdleAction::KillIdle
        );
        // Confirmed idle but not long enough yet → keep waiting.
        assert_eq!(
            idle_watchdog_decision(stall, short, Some(false)),
            IdleAction::Wait
        );
        // BUSY (a request IS running), even long past the threshold → never
        // act; reset the timer. This is the core safety constraint.
        assert_eq!(
            idle_watchdog_decision(stall, long, Some(true)),
            IdleAction::ResetTimer
        );
        // UNOBSERVABLE endpoint (no /metrics) → fail-safe: never kill.
        assert_eq!(
            idle_watchdog_decision(stall, long, None),
            IdleAction::ResetTimer
        );
        // Disabled (idle_stall == 0) → never accumulate or kill.
        assert_eq!(
            idle_watchdog_decision(Duration::ZERO, long, Some(false)),
            IdleAction::ResetTimer
        );
    }

    #[test]
    fn idle_poll_period_is_bounded() {
        let mk = |secs| GooseBuilder {
            cmd: "goose".into(),
            model: "m".into(),
            timeout: Duration::from_secs(600),
            provider: "openai".into(),
            base_url: None,
            api_key: None,
            toolshim: false,
            idle_stall: Duration::from_secs(secs),
            run_id: None,
        };
        // quarter of 120 = 30 → clamped to the 15s ceiling.
        assert_eq!(mk(120).idle_poll_period(), Duration::from_secs(15));
        // quarter of 40 = 10 → within bounds.
        assert_eq!(mk(40).idle_poll_period(), Duration::from_secs(10));
        // quarter of 4 = 1 → clamped to the 2s floor (no thrashing).
        assert_eq!(mk(4).idle_poll_period(), Duration::from_secs(2));
        // disabled → rare poll, only the wall clock matters.
        assert_eq!(mk(0).idle_poll_period(), Duration::from_secs(15));
    }

    /// F7: a goose timeout must kill the whole PROCESS GROUP, not just the
    /// direct child — a surviving grandchild is exactly the #31 orphan risk.
    #[tokio::test]
    async fn goose_timeout_kills_the_whole_process_group() {
        let dir = tempdir();
        let script = dir.join("fake-goose-hang.sh");
        // Backgrounds a long-lived grandchild (same setsid'd process group),
        // records its pid, then hangs until killed.
        std::fs::write(
            &script,
            "#!/bin/sh\nsleep 300 &\necho $! > grandchild.pid\nwait\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let b = GooseBuilder {
            cmd: script.to_string_lossy().into_owned(),
            model: "m".into(),
            // ponytail: 3s (not 500ms) so the subprocess reliably records its
            // grandchild pid before the timeout-kill under parallel test load —
            // `sleep 300` still vastly exceeds this, so the hang→timeout→kill
            // path is exercised exactly as before. Was a flake under `cargo test
            // --workspace` (all four crates' suites spawning subprocesses at once).
            timeout: Duration::from_secs(3),
            provider: "openai".into(),
            base_url: None,
            api_key: None,
            toolshim: false,
            idle_stall: Duration::from_secs(0),
            run_id: None,
        };
        let res = b.build("p", &dir).await;
        assert!(res.is_err(), "hung goose must time out");

        // Poll briefly for the pid file rather than a single racy read: under
        // load the grandchild's `echo $! > pid` can lag the parent's exit.
        let pid_path = dir.join("grandchild.pid");
        let mut pid_str = None;
        for _ in 0..50 {
            match std::fs::read_to_string(&pid_path) {
                Ok(s) if !s.trim().is_empty() => {
                    pid_str = Some(s);
                    break;
                }
                _ => std::thread::sleep(Duration::from_millis(100)),
            }
        }
        let gpid: i32 = pid_str
            .expect("grandchild pid recorded before the hang")
            .trim()
            .parse()
            .unwrap();
        // The group SIGKILL must take the grandchild down (allow reaping lag;
        // a zombie counts as dead — it can't write anything).
        let mut dead = false;
        for _ in 0..30 {
            let stat = std::fs::read_to_string(format!("/proc/{gpid}/stat"));
            match stat {
                Err(_) => {
                    dead = true;
                    break;
                }
                Ok(s) if s.contains(") Z ") => {
                    dead = true;
                    break;
                }
                Ok(_) => std::thread::sleep(Duration::from_millis(100)),
            }
        }
        assert!(dead, "grandchild pid={gpid} survived the timeout kill");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// F8 fail-safe (through the real watchdog loop): a hung goose whose
    /// endpoint is UNOBSERVABLE (no base_url → running-probe returns None)
    /// must NOT be idle-killed — it rides to the wall-clock timeout. Proves
    /// the watchdog never acts on an endpoint it can't read.
    #[tokio::test]
    async fn idle_watchdog_never_kills_an_unobservable_endpoint() {
        let dir = tempdir();
        let script = dir.join("fake-goose-hang.sh");
        std::fs::write(&script, "#!/bin/sh\nsleep 300\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let b = GooseBuilder {
            cmd: script.to_string_lossy().into_owned(),
            model: "m".into(),
            timeout: Duration::from_secs(3), // wall-clock backstop
            provider: "openai".into(),
            base_url: None, // unobservable → running-probe is None → fail-safe
            api_key: None,
            toolshim: false,
            idle_stall: Duration::from_secs(1), // would fire fast IF it could observe
            run_id: None,
        };
        let err = b.build("p", &dir).await.unwrap_err().to_string();
        assert!(
            err.contains("timed out") && !err.contains("idle-stall"),
            "unobservable endpoint must hit the WALL timeout, not idle-stall: {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// F7: reap_orphans covers goose.pid with the same contract as opencode.pid.
    #[test]
    fn reaper_cleans_dead_goose_pidfile() {
        let _cwd_guard = crate::CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempdir();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&tmp).unwrap();

        let run_dir = tmp.join(".bob/runs/r-goose");
        std::fs::create_dir_all(&run_dir).unwrap();
        // A pid that cannot exist (> kernel pid_max) — reads as dead.
        std::fs::write(run_dir.join("goose.pid"), "999999999").unwrap();

        let report = reap_orphans().unwrap();
        std::env::set_current_dir(prev).unwrap();

        assert!(report.cleaned >= 1, "dead goose pidfile counted as cleaned");
        assert!(
            !run_dir.join("goose.pid").exists(),
            "dead goose pidfile removed"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// F7: goose writes its reaper pidfile under .bob/runs/<run_id>/ and
    /// removes it on a clean exit.
    #[tokio::test]
    async fn goose_pidfile_removed_after_clean_exit() {
        let _cwd_guard = crate::CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempdir();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&tmp).unwrap();

        let script = tmp.join("fake-goose-ok.sh");
        std::fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let b = GooseBuilder {
            cmd: script.to_string_lossy().into_owned(),
            model: "m".into(),
            timeout: Duration::from_secs(5),
            provider: "openai".into(),
            base_url: None,
            api_key: None,
            toolshim: false,
            idle_stall: Duration::from_secs(0),
            run_id: Some("gpid-clean".into()),
        };
        let res = b.build("p", &tmp).await;
        let pidfile = tmp.join(".bob/runs/gpid-clean/goose.pid");
        let pidfile_exists = pidfile.exists();
        std::env::set_current_dir(prev).unwrap();

        res.unwrap();
        assert!(!pidfile_exists, "goose.pid removed after clean exit");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn times_out_a_hung_builder() {
        let b = ShimSleep {
            timeout: Duration::from_millis(200),
        };
        let res = b.build("ignored", Path::new(".")).await;
        assert!(res.is_err(), "hung builder must time out");
    }

    struct ShimSleep {
        timeout: Duration,
    }
    impl Builder for ShimSleep {
        async fn build(&self, _p: &str, _w: &Path) -> anyhow::Result<BuilderOutcome> {
            let mut child = Command::new("sleep").arg("30").kill_on_drop(true).spawn()?;
            match tokio::time::timeout(self.timeout, child.wait()).await {
                Ok(s) => {
                    s?;
                    Ok(BuilderOutcome::default())
                }
                Err(_) => {
                    let _ = child.start_kill();
                    anyhow::bail!("timed out")
                }
            }
        }
    }

    fn tempdir() -> std::path::PathBuf {
        // Atomic counter, not the clock — same-tick parallel tests collided (see doctor.rs tmp()).
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "bob-thin-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // ── SEARCH/REPLACE edit protocol ────────────────────────────────────────

    const BIG_PY: &str = "import os\n\ndef alpha():\n    return 1\n\ndef beta():\n    return 2\n\ndef gamma():\n    return 3\n";

    #[test]
    fn edit_block_applies_single_hunk() {
        let dir = tempdir();
        std::fs::write(dir.join("m.py"), BIG_PY).unwrap();
        let content = "\
=== EDIT m.py ===
<<<<<<< SEARCH
def beta():
    return 2
=======
def beta():
    return 20
>>>>>>> REPLACE
=== END ===
";
        let (written, errs) = parse_and_write_files(content, &dir).unwrap();
        assert_eq!(written, vec!["m.py"]);
        assert!(errs.is_empty(), "{errs:?}");
        let out = std::fs::read_to_string(dir.join("m.py")).unwrap();
        assert!(out.contains("return 20"));
        assert!(out.contains("def alpha()"), "rest of file preserved");
        assert!(out.contains("def gamma()"), "rest of file preserved");
    }

    #[test]
    fn edit_block_multiple_hunks_and_files_mix() {
        let dir = tempdir();
        std::fs::write(dir.join("m.py"), BIG_PY).unwrap();
        let content = "\
=== EDIT m.py ===
<<<<<<< SEARCH
def alpha():
    return 1
=======
def alpha():
    return 10
>>>>>>> REPLACE
<<<<<<< SEARCH
def gamma():
    return 3
=======
def gamma():
    return 30
>>>>>>> REPLACE
=== notes.txt ===
hello
=== END ===
";
        let (written, errs) = parse_and_write_files(content, &dir).unwrap();
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(written, vec!["m.py", "notes.txt"]);
        let out = std::fs::read_to_string(dir.join("m.py")).unwrap();
        assert!(out.contains("return 10") && out.contains("return 30"));
        assert!(out.contains("return 2"), "untouched fn preserved");
    }

    #[test]
    fn edit_block_no_match_reports_error_and_writes_nothing() {
        let dir = tempdir();
        std::fs::write(dir.join("m.py"), BIG_PY).unwrap();
        let content = "\
=== EDIT m.py ===
<<<<<<< SEARCH
def delta():
    return 4
=======
def delta():
    return 40
>>>>>>> REPLACE
=== END ===
";
        let (written, errs) = parse_and_write_files(content, &dir).unwrap();
        assert!(written.is_empty());
        assert_eq!(errs.len(), 1);
        assert!(errs[0].contains("not found"), "{errs:?}");
        assert_eq!(std::fs::read_to_string(dir.join("m.py")).unwrap(), BIG_PY);
    }

    #[test]
    fn edit_block_ambiguous_match_is_all_or_nothing() {
        let dir = tempdir();
        std::fs::write(dir.join("m.py"), "x = 1\ny = 2\nx = 1\n").unwrap();
        let content = "\
=== EDIT m.py ===
<<<<<<< SEARCH
x = 1
=======
x = 9
>>>>>>> REPLACE
=== END ===
";
        let (written, errs) = parse_and_write_files(content, &dir).unwrap();
        assert!(written.is_empty());
        assert!(errs[0].contains("2 places"), "{errs:?}");
        assert_eq!(
            std::fs::read_to_string(dir.join("m.py")).unwrap(),
            "x = 1\ny = 2\nx = 1\n",
            "file untouched on ambiguity"
        );
    }

    #[test]
    fn edit_block_lenient_on_trailing_whitespace() {
        let dir = tempdir();
        std::fs::write(dir.join("m.py"), "def f():   \n    return 1\n").unwrap();
        let content = "\
=== EDIT m.py ===
<<<<<<< SEARCH
def f():
    return 1
=======
def f():
    return 2
>>>>>>> REPLACE
=== END ===
";
        let (written, errs) = parse_and_write_files(content, &dir).unwrap();
        assert_eq!(written, vec!["m.py"]);
        assert!(errs.is_empty(), "{errs:?}");
        assert!(std::fs::read_to_string(dir.join("m.py"))
            .unwrap()
            .contains("return 2"));
    }

    #[test]
    fn search_replace_under_plain_header_is_routed_as_edit() {
        // Models forget the EDIT keyword; route by body content.
        let dir = tempdir();
        std::fs::write(dir.join("m.py"), BIG_PY).unwrap();
        let content = "\
=== m.py ===
<<<<<<< SEARCH
def beta():
    return 2
=======
def beta():
    return 22
>>>>>>> REPLACE
=== END ===
";
        let (written, errs) = parse_and_write_files(content, &dir).unwrap();
        assert_eq!(written, vec!["m.py"], "{errs:?}");
        let out = std::fs::read_to_string(dir.join("m.py")).unwrap();
        assert!(out.contains("return 22") && out.contains("def alpha()"));
    }

    #[test]
    fn whole_file_rewrite_of_oversized_existing_file_is_refused() {
        let dir = tempdir();
        // existing file bigger than the embed cap
        let big = "line\n".repeat(THIN_EMBED_WHOLE_CAP / 4);
        std::fs::write(dir.join("huge.py"), &big).unwrap();
        // model returns a tiny "complete" file — the truncation trap
        let content = "=== huge.py ===\ndef only_thing():\n    return 1\n=== END ===\n";
        let (written, errs) = parse_and_write_files(content, &dir).unwrap();
        assert!(written.is_empty(), "must not apply truncating rewrite");
        assert!(errs.iter().any(|e| e.contains("EDIT huge.py")), "{errs:?}");
        assert_eq!(
            std::fs::read_to_string(dir.join("huge.py")).unwrap(),
            big,
            "file left intact"
        );
    }

    #[test]
    fn edit_block_missing_file_suggests_whole_file_format() {
        let dir = tempdir();
        let content = "\
=== EDIT brand_new.py ===
<<<<<<< SEARCH
x
=======
y
>>>>>>> REPLACE
=== END ===
";
        let (written, errs) = parse_and_write_files(content, &dir).unwrap();
        assert!(written.is_empty());
        assert!(errs[0].contains("whole-file"), "{errs:?}");
    }

    // ── excerpt windows for oversized context files ────────────────────────

    #[test]
    fn excerpt_windows_anchor_on_definitions_and_stay_verbatim() {
        let mut lines: Vec<String> = (0..2000).map(|i| format!("filler_{i} = {i}")).collect();
        lines[1500] = "def target_helper(x):".into();
        lines[1501] = "    return x + 1".into();
        let contents = lines.join("\n");
        let prompt = "## TASK\nFix target_helper so tests pass.\n## EDITABLE PATHS\n- big.py\n";
        let idents = extract_identifiers(prompt);
        assert!(idents.iter().any(|i| i == "target_helper"), "{idents:?}");
        let out = excerpt_windows("big.py", &contents, &idents);
        assert!(out.contains("def target_helper(x):"), "window found the def");
        assert!(out.contains("EXCERPTS"), "labeled as excerpt");
        assert!(
            !out.contains("filler_0 = 0\nfiller_1 = 1"),
            "did not just embed the head"
        );
        // verbatim: the window text must be copy-pastable as SEARCH content
        assert!(out.contains("    return x + 1"));
    }

    #[test]
    fn oversized_context_file_is_excerpted_not_head_truncated() {
        let dir = tempdir();
        let mut lines: Vec<String> = (0..3000).map(|i| format!("pad_{i} = {i}")).collect();
        lines[2800] = "def wanted_fn():".into();
        std::fs::write(dir.join("huge.py"), lines.join("\n")).unwrap();
        let prompt = "## TASK\nfix wanted_fn\n## EDITABLE PATHS\n- huge.py\n## CONTEXT FILES (read-only)\n- huge.py\n";
        let enriched = enrich_with_file_contents(prompt, &dir);
        assert!(enriched.contains("def wanted_fn()"), "target visible");
        assert!(enriched.contains("EXCERPTS"), "honest excerpt label");
        assert!(!enriched.contains("pad_500 ="), "middle filler not embedded");
    }
}

// ── Reaper (unchanged) ──────────────────────────────────────────────────────

pub fn reap_orphans() -> anyhow::Result<ReapReport> {
    let mut report = ReapReport::default();
    let runs_dir = std::path::PathBuf::from(".bob/runs");
    if runs_dir.exists() {
        for entry in std::fs::read_dir(&runs_dir)? {
            let entry = entry?;
            // Both builder kinds write a reaper pidfile (goose since F7).
            for name in ["opencode.pid", "goose.pid"] {
                let pid_file = entry.path().join(name);
                if !pid_file.exists() {
                    continue;
                }
                let pid_str = std::fs::read_to_string(&pid_file)?;
                let Ok(pid) = pid_str.trim().parse::<u32>() else {
                    continue;
                };
                let alive = unsafe { libc::kill(pid as i32, 0) == 0 };
                if !alive {
                    let _ = std::fs::remove_file(&pid_file);
                    report.cleaned += 1;
                    continue;
                }
                let ppid = read_ppid(pid);
                if let Some(ppid) = ppid {
                    let parent_alive = unsafe { libc::kill(ppid as i32, 0) == 0 };
                    if !parent_alive {
                        // Builders are setsid'd — group kill reaches their
                        // grandchildren, not just the leader.
                        kill_group_with_escalation(pid);
                        let _ = std::fs::remove_file(&pid_file);
                        report.orphans_killed += 1;
                        eprintln!(
                            "reaper: killed orphan builder pid={pid} (parent {ppid} dead, {name})"
                        );
                    }
                }
            }
        }
    }
    Ok(report)
}

#[derive(Debug, Default)]
pub struct ReapReport {
    pub orphans_killed: u32,
    pub cleaned: u32,
}

fn read_ppid(pid: u32) -> Option<u32> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("PPid:") {
            return rest.trim().parse().ok();
        }
    }
    None
}
