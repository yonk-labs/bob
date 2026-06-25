# bob — Build→Verify→Judge Loop — Design Spec

**Date:** 2026-06-25 · **Status:** approved design, pre-implementation · **Author:** brainstormed with the-yonk

## TL;DR
bob is a standalone Rust binary that runs an autonomous **build → verify → judge → iterate** loop. It owns no model logic: it shells out to `opencode` (the builder — edits files) and `abe` (the judge — multi-model validation), captures each attempt as a `git diff`, runs objective gates (compile/test/lint) on it, asks abe to judge intent, and feeds critique back into the next build until the work passes or a bound is hit. It is the *worker* counterpart to abe's *judge*: abe checks work, bob produces it.

---

## 1. Purpose & non-goals

**Purpose.** Give an agent (or a human at the CLI) one call that turns a *spec/task + files* into *verified code changes*, by orchestrating an existing coding CLI and an existing validator in a closed generate-and-test loop — something neither tool does alone.

**Why bob exists (vs. running `opencode run` directly).** Running opencode once gives you an unverified diff. bob adds the **loop**: an objective verify gate + an LLM judge, with critique fed back, bounded and observable. The loop is the entire value; the build step itself is pure glue.

**Non-goals (v1).**
- Not a model provider or agent framework (opencode/abe own that).
- Not a multi-model *builder* — v1 uses a single builder (opencode). Multi-model building is a clean future extension.
- Not a workflow/orchestration engine for arbitrary tasks — it does one thing: build-verify-judge a unit of work.
- Does **not** absorb abe — it composes with it over a CLI/JSON contract, keeping abe untouched.

**Relationship to the abe roadmap.** bob's judge step is cleanest once abe ships its **structured `validate` verdict (pass/fail/uncertain)** — Phase 1 of the abe porting-roadmap. Until then, bob keys off abe's existing validate JSON (agreements/disagreements) with a heuristic, or asks abe a yes/no in-prompt and parses it. bob gives that abe feature a concrete consumer.

---

## 2. Architecture

bob is its own crate/binary. It shells out to two subprocesses and never imports their code:

- **builder** = `opencode` — run in an isolated git worktree; bob captures the resulting `git diff` (including untracked files) as the candidate.
- **judge** = `abe` — `abe validate`/`debate` over the candidate diff + spec + verify evidence; bob parses the JSON verdict.

### The loop

```
  spec/task + context files
        │
        ▼
  prepare:  secret-scan inputs (ported safety.rs)
            create throwaway git worktree at base SHA
        ▼
  ┌──────────────────────────────────────────────┐
  │ iteration i (0..max_iterations)               │
  │                                               │
  │  BUILD   prompt = spec + files                │
  │          + abe critique (i>0)                 │
  │          + verify failure output (if last     │
  │            iteration failed verify)           │
  │          run `opencode run` in worktree       │
  │          capture diff (incl. untracked)       │
  │              │                                │
  │   empty diff after a critique? → stop (stuck) │
  │              ▼                                │
  │  VERIFY  run configured gate cmds             │
  │          (cargo test / pytest / build / lint) │
  │          ├─ fail → critique = verify output   │
  │          │         → next iteration (skip abe)│
  │          └─ pass ▼                            │
  │  SCOPE   changed files/lines within caps?     │
  │          allowed paths only? else flag        │
  │              ▼                                │
  │  JUDGE   abe validate(spec, diff, verify out) │
  │          → pass | fail | uncertain + critique │
  │          ├─ pass → APPLY, done                │
  │          └─ else → critique = abe → next iter │
  └──────────────────────────────────────────────┘
        │
        ▼
  APPLY (on pass): apply candidate to real tree
        ONLY if target HEAD still == base SHA;
        else stop & report "base moved"
  non-converge: keep worktree + artifacts, report
        │
        ▼
  cleanup worktree (preserved on failure)
```

### Stopping rules (cost is bounded — each loop = 1 opencode run + 0–1 abe run)
- `verdict == pass` → apply, done.
- `i == max_iterations` (default **3**) → stop; report last diff + abe's open objections; do **not** apply.
- `max_walltime` exceeded → stop, report.
- empty diff produced after a critique → builder stuck → stop.
- repeated identical verify failure → stuck → stop.
- repeated `uncertain` verdicts → stop.

### Verify-before-judge ordering
Objective gates run **before** abe. A red gate short-circuits straight to the next iteration without spending an abe call — correctness-first ordering is cheaper and unambiguous. `verify_cmds` is configurable and **may be empty** (docs/config tasks) — then abe is the sole gate, with a logged warning.

---

## 3. Surfaces

- **CLI:** `bob build "<task>" --spec spec.md --files src/**.rs --max-iters 3 [--apply]` (default is *propose*: leaves the candidate diff for review; `--apply` merges it on pass)
- **MCP:** `bob mcp` — rmcp stdio server exposing a `build` tool. v1 is a **bounded blocking** call that returns a `RunResult` (or a run-id to inspect if it hits the time bound). Cancel/streaming deferred.
- **Setup:** `bob init` — interactive wizard (ported from abe) writing `~/.config/bob/config.yaml`.
- **Diagnostics:** `bob doctor` — checks `git`, `opencode`, `abe` are present + version-compatible, and validates config.

---

## 4. Modules

| module | responsibility |
|---|---|
| `main.rs` | dispatch |
| `cli.rs` | clap subcommands + flags |
| `config.rs` | YAML config schema + load (ported abe pattern) |
| `engine.rs` | the loop: `run(opts) -> RunResult` |
| `builder.rs` | opencode invocation: prompt assembly, subprocess contract, capture diff (incl. untracked) |
| `verify.rs` | run gate commands, capture pass/fail + output |
| `judge.rs` | abe invocation, parse JSON verdict + critique |
| `worktree.rs` | git worktree lifecycle, base-SHA capture, diff capture, apply-if-base-unchanged, keep-on-failure |
| `scope.rs` | changed-files/lines caps, optional path allowlist |
| `safety.rs` | secret-scan inputs + diff (ported from abe) |
| `report.rs` | `RunResult` → JSON/text; per-iteration artifacts |
| `mcp.rs` | rmcp stdio server; `build` tool |

---

## 5. Core data structures

```
RunOpts {
  spec: String | path,
  context_files: Vec<Path>,
  max_iterations: u32,        // default 3
  max_walltime: Duration,
  verify_cmds: Vec<String>,   // may be empty
  scope_caps: ScopeCaps { max_files, max_lines, allow_paths? },
  builder_cmd: String,        // default "opencode"
  judge_mode: Validate | Debate,
  apply: bool,                // default false (propose); true applies on pass
}

Iteration {
  index: u32,
  build_diff: String,
  verify: VerifyResult { passed: bool, output: String },
  verdict: Option<Verdict>,   // None if verify failed first
  critique: String,
  elapsed_ms: u64,
}

RunResult {
  status: Converged | NotConverged | Error,
  base_sha: String,
  iterations: Vec<Iteration>,
  final_diff: String,
  applied: bool,
  artifact_dir: Path,
}
```

---

## 6. Config (YAML, example)

```yaml
builder:
  cmd: opencode            # invoked as: opencode run <prompt> (non-interactive)
  timeout_secs: 600
judge:
  cmd: abe
  mode: validate           # validate | debate
verify:
  cmds:                    # objective gates; empty = abe-only (warned)
    - cargo test
    - cargo clippy -- -D warnings
loop:
  max_iterations: 3
  max_walltime_secs: 1800
scope:
  max_changed_files: 20
  max_changed_lines: 800
  allow_paths: []          # empty = no path restriction
apply: false               # propose by default; --apply to merge on pass
artifacts:
  dir: .bob/runs           # per-run prompt/diff/verdict/logs; kept on failure
```

---

## 7. Safety & error handling

- **Subprocess contract** (builder & judge): non-interactive / no-TTY, wall-clock + idle timeout, kill the process *group* on timeout, check OS exit code (never parse prose for status), cap stdout/stderr size, classify failure modes (rate-limit / auth / context-overflow / crash).
- **Worktree-only** (v1; no `--in-place`). Candidate is committed in the worktree; applied to the real tree **only if target HEAD still equals `base_sha`**, else stop and report "base moved" with the diff.
- **Secrets:** scan inputs before they enter prompts *and* scan the diff before apply (ported `safety.rs`); no bypass over MCP.
- **Trust model:** bob runs opencode with the **same trust the user already grants it** when running `opencode` directly — bob does not escalate. The new risk is *autonomy* (unattended loop), so bob prints a clear warning and bounds the run. A real sandbox (container/firejail) is **opt-in/future**, not v1.
- **Scope guard:** enforce `max_changed_files`/`max_changed_lines`; optional path allowlist; abe is additionally asked "does this diff *only* address the task?"
- **Observability:** per-run artifact dir (prompt, diff, verify output, verdict, logs per iteration); **preserved on failure** (cleanup only on success unless `--keep`).
- **Cost/time:** `max_iterations` + `max_walltime` + per-subprocess timeout + output caps. Budget exceeded ⇒ stop, never apply.
- **Untracked files:** capture them (stage-all / `--untracked-files`) so new files aren't silently dropped from the candidate.

---

## 8. Testing strategy

- **Unit (the heart):** engine decision logic against **fake builder + fake judge** fixtures simulating: pass, fail, uncertain, empty-diff-after-critique, subprocess timeout/hang, invalid JSON, repeated verify-fail, stuck loop, base-moved-on-apply. The loop's decision table is the product, so this gets a real (small) suite, not a single self-check.
- **Contract:** parse abe's JSON verdict shapes (current + the future structured verdict).
- **Integration (gated, like abe's):** tiny fixture git repo + real `opencode` + real `abe` — "add a function that makes this test pass"; assert convergence and applied diff.

---

## 9. Known v1 limitations (deliberately deferred)

Real execution sandbox · prompt-injection hardening (mitigated by objective gates) · loop resume from a crashed iteration · MCP cancel/status streaming (v1 is bounded-blocking + run-id) · config trust model for hostile repo-local YAML · concurrent-run locking · privacy/compliance controls for external providers · full generated-test-gaming defense (v1 only flags new test files to abe) · multi-model building.

These are documented so a new contributor knows they were considered and chosen out of scope — not missed.

---

## 10. Open dependency

bob's judge step is cleanest with abe's **structured `validate` verdict (pass/fail/uncertain + risks)** — abe porting-roadmap Phase 1. bob ships against abe's *current* JSON with a heuristic; upgrading abe later tightens the contract without changing bob's shape.

---

## 11. Rough build order (detail deferred to the implementation plan)

1. `config.rs` + `cli.rs` + `bob doctor` (skeleton, ported from abe).
2. `worktree.rs` (create / capture diff / apply-if-base-unchanged / keep-on-failure).
3. `builder.rs` (opencode subprocess contract + diff capture).
4. `verify.rs` (gate runner).
5. `judge.rs` (abe invocation + JSON parse).
6. `engine.rs` (the loop + stopping rules) — with fake builder/judge unit suite.
7. `report.rs` + artifacts.
8. `mcp.rs` (build tool).
9. Integration test (real opencode + abe).
