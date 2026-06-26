# bob

**Autonomous build → verify → judge loop.** Give bob a task and a repo; it drives a
coding CLI (`opencode`) to implement the change in an **isolated git worktree**, gates
the result on **your objective verify command** (e.g. `cargo test`), gets a **non-blocking
second opinion** from `abe`, and **applies the change only when it converges**.

bob is the *worker* counterpart to [`abe`](../debator) (the *judge*): abe checks work,
bob produces it. It owns no model logic — it orchestrates two CLIs you already have.

```
  task + repo
      │
      ▼  (in an isolated git worktree, so your tree is never touched until it passes)
  ┌──────── loop, up to --max-iters ────────────────────────────┐
  │  BUILD   opencode edits files in the worktree               │
  │  scope   changed files/lines within caps?                   │
  │  VERIFY  run your gate (cargo test / npm test / …)          │
  │            ├─ fail → feed the failure back → next iteration │
  │            └─ pass ▼                                        │
  │  JUDGE   abe gives a non-blocking advisory second opinion   │
  │  →  CONVERGED: apply the candidate to your real tree        │
  └─────────────────────────────────────────────────────────────┘
```

The **verify gate is the authority** — a passing objective check (your tests) is what
makes bob converge. abe's review is surfaced as advice but never blocks.

---

## Why not just run `opencode` directly?

Running a coding agent once gives you an *unverified* edit straight into your working tree.
bob adds the loop around it: an **isolated worktree** (nothing touches your files until it
passes), an **objective gate** (your tests must go green), **bounded** iteration with stuck
detection, a **second opinion**, and an **apply gate** (propose by default). The loop is the
value; the build step itself is just glue.

---

## Install

**Prerequisites** (bob shells out to these):
- `git`
- [`opencode`](https://opencode.ai) — the builder CLI, configured with a model.
- [`abe`](../debator) — the judge CLI (`abe init` to configure). *(Optional in practice —
  the judge is advisory; if abe is missing the run still converges on the verify gate.)*

```bash
# from the repo
cargo install --path .          # installs `bob` to ~/.cargo/bin
# or
./install.sh                    # builds release + copies to ~/.local/bin

bob doctor                      # checks git / opencode / abe / config
```

## Quick start

```bash
cd your-project
bob init                        # writes a starter ./bob.yaml
$EDITOR bob.yaml                # set verify.cmds to THIS project's test command
bob doctor                      # confirm tools + config

# Propose a change (default — leaves a candidate diff, your tree untouched):
bob build "Make the failing auth test pass"

# Apply it on convergence:
bob build "Make the failing auth test pass" --apply
```

---

## Configuration (`bob.yaml`)

Searched as `./bob.yaml` then `~/.config/bob/config.yaml` (override with `--config`).

```yaml
builder:
  cmd: opencode           # builder CLI; invoked as: <cmd> run --dir <wt> --model <id> <args> <prompt>
  timeout_secs: 600       # per build-step wall-clock timeout
  model: qwen             # default model: a name from `models`, or a raw provider/model id
  models:                 # named roster — switch with `bob build --model <name>`, list with `bob models`
    qwen:    ollama/Intel/Qwen3-Coder-Next-int4-AutoRound
    minimax: minimax/MiniMax-M3
  args: []                # extra opencode flags (not the model), e.g. ["--variant", "high"]
judge:
  cmd: abe                # judge CLI; invoked as: <cmd> validate --json -- <statement>
  mode: validate          # validate | debate
  timeout_secs: 600
verify:
  cmds:                   # objective gate(s); run in order, stop at first failure.
    - cargo test          # empty list => no gate (bob warns; converges on first diff)
loop:
  max_iterations: 3
  max_walltime_secs: 1800
scope:
  max_changed_files: 20   # reject a runaway diff
  max_changed_lines: 800
  allow_paths: []         # [] = no path restriction; else only these prefixes may change
apply: false              # propose by default; CLI --apply overrides
artifacts:
  dir: .bob/runs          # per-iteration prompt/diff/verdict (gitignore this)
```

**Choosing the builder's model.** Keep a named roster in `builder.models` (name → `provider/model`
id, from `opencode models`) and set the default with `builder.model`. Switch per run with
`bob build --model <name-or-id>` (MCP: a `model` param), and list the roster with `bob models`.
A `--model` value that isn't a roster name is passed through as a raw id. Omit `builder.model`
entirely to use opencode's own default. The *judge's* models live in abe's config (`abe.yaml`), not here.

**Guardrails.** bob enforces several, all from `bob.yaml`:
- **Verify gates** (`verify.cmds`) are your extensible guardrail — *any* shell command that
  must pass. Add lints/scanners/policy checks: `["cargo test", "cargo clippy -- -D warnings",
  "./check-policy.sh"]`. If any fails, bob doesn't converge.
- **Scope** — `scope.max_changed_files` / `max_changed_lines` cap blast radius;
  `scope.allow_paths: ["src/"]` restricts *which* paths may change (anything outside stops the run).
- **Secret scan** on inputs + the diff, **propose-by-default** (no `--apply` = no writes),
  and bounded iteration (`max_iterations` / `max_walltime_secs`).

## CLI

```
bob build <task> [--spec FILE] [--files ...] [--max-iters N] [--apply] [--keep]
bob doctor            # check git/opencode/abe presence + config
bob init              # write a starter ./bob.yaml
bob mcp               # run the stdio MCP server
```

- `--apply` — apply the candidate to your working tree on convergence (default: propose only).
- `--spec FILE` — use a file's contents as the task/spec (secret-scanned first).
- `--files ...` — context file paths to mention in the build prompt.
- `--max-iters N` — override the config's loop cap.
- `--keep` — keep the worktree + artifacts even on success (for inspection).

**Exit codes:** `0` converged, `1` did not converge / error. (So CI and agents can detect failure.)

## MCP server

`bob mcp` is a stdio MCP server exposing one tool, `build`, with params
`{ task, spec?, files?, max_iters?, apply? }`, returning the `RunResult` as JSON.
`apply` defaults to **false** over MCP — a host agent can never trigger an auto-apply by
omitting the field. Register it like any stdio MCP server (command `bob`, arg `mcp`).

## Use it from Claude Code / Codex (plugin)

bob ships as a Claude Code plugin: the `build` MCP tool, a `/bob:build` command, and a
**delegation skill** (`delegating-to-bob`) that teaches the host agent when and how to hand
implementation work to bob.

```
/plugin marketplace add yonk-labs/bob      # or: /plugin marketplace add /path/to/bob (local)
/plugin install bob@yonk-labs
```

A coding agent can then delegate a verified build — `/bob:build make the failing auth test pass`,
or (once the skill triggers) call the `build` tool directly and get back a `RunResult`
(status, applied, final_diff). The plugin runs `bob mcp`, so `bob` must be installed and on PATH
first (see Install). Non-Claude MCP clients (Codex, opencode) register the stdio server directly:
command `bob`, arg `mcp`.

---

## Use cases

- **Auto-fix failing tests.** Point bob at a repo whose tests fail and set `verify.cmds` to
  the test command. bob loops opencode until the suite goes green, then applies.
  `bob build "fix the failing tests" --apply`
- **Implement from a spec, verified.** `bob build "implement the parser" --spec parser.md
  --files src/parser.rs` — bob iterates until your gate passes, so you get *working* code,
  not just a plausible diff.
- **Verified codegen for agents (MCP).** A host agent (Claude Code, etc.) calls bob's `build`
  tool to autonomously implement + verify a unit of work and get back a structured result —
  offloading the build loop without polluting its own context.
- **Cross-model building.** Your host is one model; point bob's builder at a *different* model
  via opencode (e.g. a local coder model) — bob orchestrates the hand-off and verification.
- **Safe "propose" review.** Run without `--apply` to get a verified candidate diff to review
  before it touches your tree — a tested suggestion, not a blind edit.

## Safety model

- **Worktree isolation.** Edits happen in a throwaway git worktree under `.bob/worktrees/`;
  your working tree is untouched until convergence, and applied only if its `HEAD` is unchanged.
- **Propose by default.** No `--apply`, no changes to your tree.
- **Secret scanning.** The spec/task and the candidate diff are scanned for credential markers;
  a hit aborts (inputs) or blocks the apply (diff).
- **Scope caps.** A diff exceeding `max_changed_files`/`max_changed_lines` (or touching paths
  outside `allow_paths`) stops the run instead of applying a runaway change.
- bob runs `opencode` with the same trust you already grant it; it does not escalate.

## How convergence is decided (stopping rules)

- **Converged** — the verify gate passed (objective authority). abe's take is advisory.
- **Did not converge** — `max_iterations` reached, walltime exceeded, builder produced no diff
  after a critique (stuck), the same verify failure repeated, or the diff exceeded scope caps.

## Known limitations

- `bob init` writes a starter config; it is not yet an interactive wizard.
- Builder/judge invocation assumes `opencode`/`abe` conventions (`run --dir`, positional
  statement); other CLIs need a shim.
- abe `validate` returns prose (it's an adversarial critic), so it is **advisory** here — the
  verify gate decides. A future structured abe verdict (its roadmap "Phase 1") could let abe
  gate again.
- Scope's changed-file count is text-diff based; binary-only changes can be undercounted.

## Layout

`src/engine.rs` (the loop + pure decision logic) · `builder.rs`/`judge.rs`/`verify.rs`
(the three steps) · `worktree.rs` (isolation + apply) · `scope.rs`/`safety.rs` (guards) ·
`mcp.rs` (MCP server) · `report.rs` (output + artifacts). Design + plan in
`docs/superpowers/`.
