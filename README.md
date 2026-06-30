# bob

**Autonomous build → verify → judge loop.** Give bob a task and a repo; it drives a
builder CLI (`goose` by default, `opencode` for the frontier tier) to implement the change in an **isolated git worktree**, gates
the result on **your objective verify command** (e.g. `cargo test`), optionally uses
`abe` critique according to `judge.policy`, and **applies the change only when it converges**.

bob is the *worker* counterpart to [`abe`](../debator) (the *judge*): abe checks work,
bob produces it. It owns no model logic — it orchestrates two CLIs you already have.

```
  task + repo
      │
      ▼  (in an isolated git worktree, so your tree is never touched until it passes)
  ┌──────── loop, up to --max-iters ────────────────────────────┐
  │  BUILD   goose/opencode edits files in the worktree         │
  │  scope   changed files/lines within caps?                   │
  │  VERIFY  run your gate (cargo test / npm test / …)          │
  │            ├─ fail → feed the failure back → next iteration │
  │            └─ pass ▼                                        │
  │  JUDGE   abe advises, blocks, or feeds retry per policy      │
  │  →  CONVERGED: apply the candidate to your real tree        │
  └─────────────────────────────────────────────────────────────┘
```

The **verify gate is the primary authority**. By default Abe is advisory; set
`judge.policy: blocking` to require Abe to pass, or `retry_on_fail` to feed Abe critique
back into the builder.

---

## Agent Lifecycle

Bob is intentionally narrow: it builds one bounded slice and reports exactly what happened.
It works best when another agent or human has already turned the request into a precise
behavior contract.

- **Frontier orchestrator** decides product behavior, sequencing, risk, and when to stop.
- **Greta** should settle UX and usability criteria before test/spec work starts.
- **Hector** turns the decision into Bob-sized slices with verify commands, editable paths,
  reference paths, and scope caps.
- **Bob** implements each slice in an isolated worktree and returns structured results.
- **Abe** reviews or blocks depending on `judge.policy`; `needs_review` is handed back to
  the orchestrator rather than silently accepted.

The practical loop is:

```bash
hector frontier-brief
hector plan ... --out campaign.yaml
hector check --file campaign.yaml
bob campaign --file campaign.yaml
hector review --campaign campaign.yaml --bob-result .bob/runs/campaign-*-result.json
```

If Hector says `split_task`, split the behavior before invoking Bob again. If Bob reports
`needs_review`, compare Abe's critique and the final diff against Hector's original contract.

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
- [`goose`](https://github.com/block/goose) — the default builder CLI, used for the
  `cheap`/`medium`/`large` tiers (and any tier-less config with `cmd: goose`).
- [`opencode`](https://opencode.ai) — the heavier builder CLI used for the `frontier`
  tier. *(Optional — only required if your `bob.yaml` routes a tier to opencode.
  `bob doctor` flags whichever builder your config actually needs.)*
- [`abe`](../debator) — the judge CLI (`abe init` to configure). *(Optional when
  `judge.policy: advisory`; required for `blocking` or `retry_on_fail`.)*

```bash
# from the repo
cargo install --path .          # installs `bob` to ~/.cargo/bin
# or
./install.sh                    # builds release + copies to ~/.local/bin

bob doctor                      # checks git / opencode / abe / goose (if used) / config
```

**Opencode installation** (opencode must be available to bob):
```bash
# Official installer
curl -fsSL https://opencode.ai/install | bash

# Alternative: npm
npm install -g opencode-ai

# Alternative: Homebrew
brew install anomalyco/tap/opencode
```

If `bob doctor` reports opencode missing, the installer suggests the above options.

**Tier builders & local endpoints.** Tiers pick the builder: `cheap` → thin
(direct curl, single-shot), `medium`/`large` → goose (agent loop), `frontier` →
opencode. The thin and goose builders talk to an OpenAI-compatible endpoint; for
local models (model ids prefixed `ollama/` or a `192.168.x.x/…` host) the base URL
defaults to a local vLLM server. Override it with `BOB_VLLM_URL` (e.g.
`export BOB_VLLM_URL=http://your-host:8000/v1` — scheme and `/v1` are added if you
omit them). Cloud ids (`minimax…`, `zai…`) use their provider URL and read the
matching `*_API_KEY` env var.

## Quick start — interactive installer

```bash
cd your-project
bob init                        # interactive wizard: detect tools, prompt for config
  # prompts for: builder cmd/model, judge cmd/mode, verify cmds, loop limits,
  # scope caps, apply default, artifacts dir — then writes bob.yaml
bob doctor                      # confirm tools + config
```

## Quick start — manual config

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
    qwen:    ollama/Intel/Qwen3-Coder-Next-int4-AutoRound   # legacy form: provider/model id
    # Explicit form (same shape as hector.yaml / abe.yaml) — gives the thin/goose
    # builders an exact endpoint instead of guessing from the id prefix:
    local:   { model: "Intel/Qwen3-...", base_url: "http://192.168.1.193:8000/v1" }
    minimax: { model: "MiniMax-M3", base_url: "https://api.minimax.io/v1", api_key_env: MINIMAX_API_KEY }
  fallback_models: []     # roster aliases or raw ids; example ["minimax"] resolves above
  args: []                # extra opencode flags (not the model), e.g. ["--variant", "high"]
judge:
  cmd: abe                # judge CLI. validate = one reviewer (light); debate = multi-model panel (heavy)
  mode: validate          # validate | debate. Both yield a structured pass/fail/uncertain verdict.
  timeout_secs: 600
  policy: advisory        # advisory (verify gate is authority; abe is a non-blocking second opinion)
                          # | blocking (abe must pass) | retry_on_fail (feed abe critique back to builder)
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
Set `builder.fallback_models` or pass `--fallback-model <name-or-id>` to retry on builder errors
or clear stuck results (`EmptyDiffAfterCritique`, repeated verify failure). Fallback entries are
either roster aliases from `builder.models` or raw provider/model ids; `bob doctor` warns on likely
alias typos.

**Model selection & stats — how bob prioritizes.** Within a tier, bob doesn't try models in
config order — it **re-ranks them every run by measured performance**. Config order is only the
cold-start default and the tie-breaker.

After every attempt, bob appends to `.bob/model-stats.json` (per-project, gitignored, created on
first run, flock-serialized so parallel bob runs don't clobber it):

```json
{
  "models": {
    "ollama/Intel/Qwen3-Coder-Next-int4-AutoRound": {
      "runs": 10, "successes": 9, "avg_latency_secs": 40.0,
      "last_latency_secs": 38.2, "last_success": true
    },
    "192.168.1.133/cyankiwi/gemma-4-26B-A4B-it-AWQ-4bit": {
      "runs": 10, "successes": 3, "avg_latency_secs": 20.0,
      "last_latency_secs": 21.1, "last_success": false
    }
  }
}
```

Each model gets a **score**, and the tier's chain is sorted by it, highest first:

```
score = success_rate × (1 / avg_latency_secs) × 100      # reliability × speed
```

So with the stats above (tier `medium: [gemma, qwen]`):

| model | success_rate | avg latency | score | rank |
|-------|-------------|-------------|-------|------|
| qwen  | 9/10 = 0.90 | 40s | `0.90 × 1/40 × 100` = **2.25** | 1st |
| gemma | 3/10 = 0.30 | 20s | `0.30 × 1/20 × 100` = **1.50** | 2nd |

qwen runs first despite being 2× slower — reliability outweighs raw speed. You see the result in
the run log: `bob: tier='medium' chain (ranked by stats): [qwen, gemma]`. A flaky or dead model
sinks on its own; a fast reliable one floats up. An **unseen** model is neutral (success_rate 0.5,
assumed 45s latency → score ≈ 1.1), so it's tried but not blindly trusted.

Two more stat-driven behaviors fall out of the same data:
- **Adaptive timeout** = `2 × avg_latency`, clamped to `[30s, 180s]`. It only ever *raises* your
  configured `timeout_secs` for a known-slow model — never lowers it.
- **Health check** — a ~3s endpoint ping before a local model is attempted, so a down endpoint is
  skipped instead of burning the timeout.

**Inspect & reset.** `bob stats` prints the current standings (runs, success %, avg/last latency,
score), sorted by score:

```
model                                          runs  succ%    avg_s   last_s  score
ollama/Intel/Qwen3-Coder-Next-int4-AutoRound      10    90%    40.0s    38.2s    2.3
192.168.1.133/cyankiwi/gemma-4-26B-...            10    30%    20.0s    21.1s    1.5
```

`bob stats --reset` deletes `.bob/model-stats.json` so rankings start cold again.

**Steering it.** By default priority is *learned* (the score above), but three `builder` knobs
let you override it:

```yaml
builder:
  reliability_weight: 0.5   # 0.0 = pure speed · 0.5 = balanced (default) · 1.0 = pure reliability
  pin: [gemma]              # always tried FIRST, in this order, ahead of stats ranking
  exclude: [minimax]        # never attempted — dropped from every tier chain
```

- **`reliability_weight`** re-biases the score: `reliability^(2w) × speed^(2(1-w))`. At `0.5` it's
  exactly the balanced formula (default, nothing changes); raise it toward `1.0` to prefer models
  that *succeed* even if slower, lower it toward `0.0` to prefer the *fastest* regardless of flakiness.
- **`pin`** / **`exclude`** are hard overrides (roster alias or raw id). `pin` forces models to the
  front of the chain; `exclude` removes them entirely. `pin` wins if a model is in both.

You can also `bob stats --reset` to wipe learned history (e.g. after fixing a flaky endpoint that
unfairly tanked a model's score). `bob stats` shows scores under your configured `reliability_weight`.

**Guardrails.** bob enforces several from `bob.yaml`, with task-local CLI/MCP overrides:
- **Verify gates** (`verify.cmds`) are your extensible guardrail — *any* shell command that
  must pass. Add lints/scanners/policy checks: `["cargo test", "cargo clippy -- -D warnings",
  "./check-policy.sh"]`. If any fails, bob doesn't converge.
- **Scope** — `scope.max_changed_files` / `max_changed_lines` cap blast radius;
  `scope.allow_paths: ["src/"]` restricts *which* paths may change (anything outside stops the run).
- **Judge policy** — `advisory` preserves verify-authority behavior; `blocking` requires Abe
  to pass; `retry_on_fail` feeds Abe critique back into the builder.
- **Secret scan** on inputs + the diff, **propose-by-default** (no `--apply` = no writes),
  and bounded iteration (`max_iterations` / `max_walltime_secs`).

**Project lessons.** If `.bob/lessons.md` exists, bob includes it in builder and judge
context so repeated local pitfalls are not rediscovered every run. Keep it short and factual;
bob refuses to use it if it is over 16KB or trips the secret scanner.

## CLI

```
bob build <task> [--spec FILE] [--files ...] [--max-iters N]
  [--verify CMD] [--allow-path PATH] [--max-changed-files N]
  [--max-changed-lines N] [--judge-policy advisory|blocking|retry_on_fail]
  [--model NAME_OR_ID] [--fallback-model NAME_OR_ID] [--apply] [--keep-worktree]
bob doctor            # check git/opencode/abe presence + config
bob init              # write a starter ./bob.yaml
bob mcp               # run the stdio MCP server
bob gc [--dry-run]    # remove stale .bob/worktrees/* and bob/* branches
bob campaign --file campaign.yaml
```

- `--apply` — apply the candidate to your working tree on convergence (default: propose only).
- `--spec FILE` — use a file's contents as the task/spec (secret-scanned first).
- `--files ...` — context file paths to mention in the build prompt.
- `--max-iters N` — override the config's loop cap.
- `--verify CMD` — override verify gates for this run; repeat for multiple gates.
- `--allow-path PATH` — restrict this run's editable paths; repeat for multiple prefixes.
- `--max-changed-files N` / `--max-changed-lines N` — override scope caps for this run.
- `--judge-policy ...` — override whether Abe is advisory, blocking, or retry feedback.
- `--model NAME_OR_ID` — override the builder model for this run.
- `--fallback-model NAME_OR_ID` — fallback builder model for errors/stalls; repeat for a chain.
- `--keep` / `--keep-worktree` — keep the worktree after the run. Artifacts are always kept.

**Cleanup.** `bob gc --dry-run` shows stale Bob worktrees and `bob/*` branches; `bob gc`
removes them. Use it after interrupted or non-converged runs if later tooling trips over
`.bob/worktrees`. Normal completed runs clean their worktree by default, including
non-converged runs; inspect `artifact_dir` and `final_diff` instead.
For JS/Jest repos, `bob doctor` warns if `.gitignore` does not ignore `/.bob`.

**Exit codes:** `0` converged, `1` did not converge / error. (So CI and agents can detect failure.)

## Campaigns

`bob campaign --file campaign.yaml` drains a serial list of Bob-sized slices. Multi-slice
campaigns require `auto_commit: true`, so each slice becomes the next slice's real git base.
The working tree must be clean before an auto-commit campaign starts. This is the preferred
surface for Hector output: tests/specs go in `reference_paths`, production files go in
`editable_paths`, and each slice carries its own verify command and scope caps.

```yaml
name: roster-plan-api
auto_commit: true          # implies apply; creates one commit per converged slice
slices:
  - name: summary endpoint
    task: Implement GET /api/roster-plan so the focused test passes.
    verify_cmds:
      - npx jest tests/routes/roster-plan-summary.test.js
    editable_paths:
      - src/routes/api/
      - src/app.js
    reference_paths:
      - tests/routes/example.test.js
    judge_policy: retry_on_fail
```

Each slice may override `verify_cmds`, `editable_paths`/`allow_paths`, scope caps,
`judge_policy`, `model`, and `fallback_models`. Campaign output is JSON with per-slice status,
changed files, artifact directory, final diff, and `result_path`. Bob also writes the same JSON
to that path under `.bob/runs/`; feed it to `hector review` when Hector created the campaign.

## MCP server

`bob mcp` is a stdio MCP server exposing one tool, `build`, with params
`{ task, spec?, files?, max_iters?, verify_cmds?, allow_paths?, max_changed_files?,
max_changed_lines?, judge_policy?, model?, fallback_models?, apply?, keep_worktree? }`, returning the `RunResult` as JSON.
`apply` defaults to **false** over MCP — a host agent can never trigger an auto-apply by
omitting the field. Register it like any stdio MCP server (command `bob`, arg `mcp`).

The bundled Codex/opencode plugin MCP config registers `bob mcp`, `abe mcp`, and `hector mcp`.

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
(status, applied, next_action, verify/judge/scope details, final_diff). The plugin runs
`bob mcp`, so `bob` must be installed and on PATH first (see Install). Non-Claude MCP clients
(Codex, opencode) register the stdio server directly: command `bob`, arg `mcp`.

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

## Interactive installer wizard

When you run `bob init`, the installer:

1. **Detects tools** — checks for `git`, `opencode`, `abe` on PATH
2. **Prompts for configuration:**
   - Builder command (default: opencode)
   - Builder model roster (optional, named models with default)
   - Builder timeout (default: 600s)
   - Builder extra args (default: none)
   - Judge command (default: abe)
   - Judge mode: validate | debate (default: validate)
   - Judge timeout (default: 600s)
   - Verify commands (objective gates, all must pass)
   - Max iterations (default: 3)
   - Max walltime (default: 1800s)
   - Max changed files (default: 20)
   - Max changed lines (default: 800)
   - Allow paths (restrict which paths may change; empty = anywhere)
   - Apply by default (default: propose-only)
   - Artifacts directory (default: .bob/runs)
3. **Writes `bob.yaml`** — the complete configuration
4. **Guides next steps** — `bob doctor` to verify

Opencode missing? The wizard prints:

```
Official install: curl -fsSL https://opencode.ai/install | bash
Alternative: npm install -g opencode-ai
Alternative: brew install anomalyco/tap/opencode
```

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

- **Converged** — the verify gate passed and the configured `judge.policy` is satisfied.
- **Did not converge** — `max_iterations` reached, walltime exceeded, builder produced no diff
  after a critique (stuck), the same verify failure repeated, the judge policy rejected it,
  or the diff exceeded scope caps.

## Known limitations

- Builder/judge invocation assumes `opencode`/`abe` conventions (`run --dir`, positional
  statement); other CLIs need a shim.
- abe `validate` can return prose-only `uncertain` output; `advisory` treats that as advice,
  while `blocking` and `retry_on_fail` enforce the configured policy.
- Scope's changed-file count is text-diff based; binary-only changes can be undercounted.

## Layout

`src/engine.rs` (the loop + pure decision logic) · `builder.rs`/`judge.rs`/`verify.rs`
(the three steps) · `worktree.rs` (isolation + apply) · `scope.rs`/`safety.rs` (guards) ·
`mcp.rs` (MCP server) · `report.rs` (output + artifacts). Design + plan in
`docs/superpowers/`.
