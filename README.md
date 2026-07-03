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

## What's new in 0.4.0

- **`worktree.setup_cmds`.** Commands run once in every fresh worktree (build, replay,
  `bob replay`/`bob apply`) before iteration 0 — never in the main tree. `BOB_REPO_ROOT`
  is exported, so JS repos can `ln -sfn "$BOB_REPO_ROOT/node_modules" node_modules`
  instead of hand-writing fragile verify-cmd hacks. A failing setup cmd aborts the run
  as an infra error (no model escalation, not a verify/judge fail).
- **`bob doctor --probe`.** Curls each distinct configured endpoint (`<base_url>/models`,
  3s timeout, deduped) and marks DEAD entries with the models/tiers that resolve there —
  no more silently routing around a dead box without knowing it.
- **`verify.focused_cmds`.** Opt-in fast per-iteration gate; the full `verify.cmds` still
  run at replay-verify and gate the run. Cuts two of three full-suite runs per converged
  run. Ignored (with a warning) unless `replay: true` and `cmds` is non-empty.
- **`builder.cmd` optional with tiers.** When `tiers:` has entries, `cmd` defaults to
  `goose` — tiers already pick the builder per tier.
- **MCP `tier` param.** The MCP `build` tool accepts `tier` like the CLI's `--tier`
  (an explicit `model` pin is still tried first).
- **`bob models --json`.** Machine-readable tier→endpoint map (default model/tier,
  tiers with their model lists, per-model id + explicit `base_url` or null — never
  guessed). Built for orchestrators: dispatch round-robin across endpoints,
  escalation-ladder sync, pre-dispatch tier checks.
- **`verify_start` event.** `events.jsonl` now marks the moment a run enters its
  verify phase — the unambiguous "builder endpoint is free" signal an orchestrator
  needs to pipeline the next slice's builder.
- Doctor also now acknowledges tier-less `cmd: goose` correctly and, for JS repos, warns
  to exclude `.bob/**` in the test runner (not just `.gitignore`) so kept worktrees don't
  double your suite.

## What's new in 0.3.0

- **Replay-verify.** Converged runs now re-apply the final diff to a fresh worktree at
  base and re-run the verify gates there before reporting `converged` — catches gates
  that only passed because of leftover builder-worktree state. On by default
  (`verify.replay: true`); a replay failure reports `ReplayVerifyFailed` /
  `human_decision_required` instead of applying blind. See `bob replay` / `bob apply`.
- **Context ceilings.** `context.soft_tokens` / `context.hard_tokens` in `bob.yaml` cap
  the estimated prompt size sent to the builder — warn past soft, refuse the run past
  hard. `RunResult.context_est_tokens` and per-iteration `prompt_est_tokens` report
  what was actually sent.
- **Worktrees are kept by default** on any non-converged outcome (previously reaped
  the same as converged runs), so you can inspect what the builder actually did;
  `bob gc` reaps them when you're done.
- **Editable test paths.** Test files listed under `allow_paths`/`editable_paths` are no
  longer silently reverted — the builder may edit them as part of the deliverable. Any
  test-file resets that *do* happen (paths outside that exemption) are reported in
  `RunResult.reset_test_files`.
- **Telemetry.** Every run writes `<artifacts>/<run_id>/run.json` (the full `RunResult`)
  and `events.jsonl` (one timestamped JSON event per loop step).
- **Campaign integration gate.** A campaign file's top-level `verify_cmds` runs once
  after all slices land, catching cross-slice interactions no single slice's gate would see.

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
local models the base URL comes from the model's `base_url` roster entry, from a
`192.168.x.x/…` host prefix in the id, or from `BOB_VLLM_URL` (e.g.
`export BOB_VLLM_URL=http://your-host:8000/v1` — scheme and `/v1` are added if you
omit them). A bare or `ollama/`-prefixed id with none of those is an error — bob
never guesses an endpoint. Cloud ids (`minimax…`, `zai…`) use their provider URL
and read the matching `*_API_KEY` env var.

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
    local:   { model: "Intel/Qwen3-...", base_url: "http://your-vllm-host:8000/v1" }
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
  replay: true            # re-apply the final diff to a fresh worktree at base and
                          # re-run the gates there before reporting converged (default: true)
loop:
  max_iterations: 3
  max_walltime_secs: 1800
context:
  soft_tokens: 16000       # warn when the estimated builder prompt exceeds this
  hard_tokens: 32000       # refuse the run outright past this
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
- **Replay-verify** (`verify.replay`, default on) — before reporting converged, bob
  re-applies the final diff to a fresh worktree at base and re-runs the verify gates
  there. A replay failure stops the run (`ReplayVerifyFailed`) instead of applying.
- **Context ceilings** (`context.soft_tokens` / `hard_tokens`) — bob estimates the
  builder prompt size before each attempt; past `soft_tokens` it warns, past
  `hard_tokens` it refuses the run rather than send a prompt local models choke on.
- **Frozen tests, with an exemption** — test files are frozen by default; listing one
  under `editable_paths`/`allow_paths` makes it part of the deliverable. Any test file
  the builder touches outside that exemption is reset to its base_sha state and
  reported in `RunResult.reset_test_files`.

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
bob replay <run_id>   # re-run replay-verify for a past run, read-only
bob apply <run_id>    # replay-verify, then git-apply the run's diff to your working tree
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

**Cleanup.** Converged runs clean up their worktree automatically; any non-converged
outcome (`not_converged`, `needs_review`, `error`) **keeps** its worktree by default so
you can inspect what the builder actually did — `bob gc --dry-run` shows stale Bob
worktrees and `bob/*` branches, `bob gc` removes them. Pass `--keep`/`--keep-worktree`
to keep a converged run's worktree too.
For JS/Jest repos, `bob doctor` warns if `.gitignore` does not ignore `/.bob`.

**Replay & apply.** `bob replay <run_id>` re-applies a past run's recorded `final_diff`
to a fresh worktree at its `base_sha` and re-runs the verify gates — read-only, exits
`1` on failure. `bob apply <run_id>` does the same replay-verify, then `git apply`s the
diff to your current working tree (unstaged); it refuses if `HEAD` has moved off the
run's `base_sha` rather than apply against a moved target. Useful for an unattended run
that reported `converged` but you want to double-check before trusting the diff, or one
where `apply: false` left a candidate you're now ready to use:

```bash
bob build "fix the flaky retry test" --max-iters 5   # unattended, apply: false
# ... later, sanity-check before trusting the candidate:
bob replay a1b2c3d4                                  # re-verify in a clean worktree
bob apply a1b2c3d4                                   # replay passes -> apply to working tree
```

**Exit codes:** `0` converged, `1` did not converge / error. (So CI and agents can detect failure.)

## Telemetry

Every run writes two files under `<artifacts.dir>/<run_id>/`, alongside the existing
per-iteration `iter-N/{prompt.txt,diff.patch,verdict.txt}`:

- **`run.json`** — the full `RunResult` as JSON: status, next_action, verify/judge/scope
  detail, `final_diff`, `verify_cmds`, `context_est_tokens`, `reset_test_files`, and more.
  This is what `bob replay`/`bob apply` read back in.
- **`events.jsonl`** — one timestamped JSON object per loop step (build attempt, verify
  result, test-file resets, judge verdict, replay-verify, run end), so you can reconstruct
  a run's timeline without re-parsing prose logs. Best-effort — a write failure here never
  fails the run.

## Campaigns

`bob campaign --file campaign.yaml` drains a serial list of Bob-sized slices. Multi-slice
campaigns require `auto_commit: true`, so each slice becomes the next slice's real git base.
The working tree must be clean before an auto-commit campaign starts. This is the preferred
surface for Hector output: tests/specs go in `reference_paths`, production files go in
`editable_paths`, and each slice carries its own verify command and scope caps.

```yaml
name: roster-plan-api
auto_commit: true          # implies apply; creates one commit per converged slice
verify_cmds:
  - npm run test:all        # integration gate — runs once after all slices land
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

A top-level `verify_cmds` (as opposed to a slice's) is an **integration gate**: once all
slices land, it runs once against the fully-merged tree, in `auto_apply`/`auto_commit`
campaigns only. It catches interactions between slices that no individual slice's verify
command would see. Failure reports campaign status `integration_failed` instead of the
per-slice statuses.

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

## Self-hosted / local models

Running the goose builder against your own OpenAI-compatible endpoint (vLLM, MLX
server, ollama) works, but a few requirements aren't obvious. Most failures show
up as a single silent symptom: `NOT CONVERGED … EmptyDiffAfterCritique`. That
almost always means **the builder never made a tool call** — no edits, empty
diff. Check these in order:

1. **The endpoint must return structured `tool_calls`.** goose acts only on
   OpenAI-style structured tool calls; a server that emits the call as *text*
   produces an empty diff. Pre-flight a server before trusting it:
   ```bash
   curl -s $HOST/v1/chat/completions -d '{"model":"…","messages":[{"role":"user","content":"Create hello.txt containing hello. Use the write tool."}],"tools":[{"type":"function","function":{"name":"write","parameters":{"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"]}}}]}' | jq '.choices[0].message.tool_calls'
   ```
   If that `tool_calls` is `null`, structured tool-calling isn't reaching goose —
   see the remedies below. Grammar-constrained / dedicated-parser servers (vLLM
   with a tool-call parser ✅, MLX server ✅) are the reliable choice. Ollama is
   template-driven: it only extracts `tool_calls` when the chat template selects a
   parser for that model/build (check its log's `template selection` line for a
   non-empty `parser=`), and it has streaming gaps with tools — prefer
   `stream:false`. The pre-flight curl catches every variant regardless of cause.
   If the server can't return structured calls, set `GOOSE_TOOLSHIM=true` (below).
2. **`GOOSE_TOOLSHIM=true` — text-output fallback.** Makes goose interpret tool
   calls from plain-text output, so a parser-less server works with no server or
   model change (adds an interpretation step per call). Enable it in `bob.yaml`:
   ```yaml
   builder:
     goose_toolshim: true      # sets GOOSE_TOOLSHIM=true for the goose builder
   ```
   or ad hoc: `GOOSE_TOOLSHIM=true bob build …` (bob inherits the environment).
3. **Use a tool-calling *coder* model.** Qwen3-Coder-Next is verified. Thinking-mode
   generalists (gemma-family, qwen-thinking) route everything to the reasoning
   channel over OpenAI-compat and return empty content unless thinking is disabled
   server-side — same empty-diff symptom.
4. **HF-style ids (with a `/`) need the roster Full form.** A bare tier string like
   `Intel/Qwen3-Coder-Next-int4-AutoRound` is passed through intact as `--model`,
   but bob can't derive its `base_url` from the id. Give it the endpoint explicitly:
   ```yaml
   builder:
     cmd: goose
     models:
       qwenc:
         model: "Intel/Qwen3-Coder-Next-int4-AutoRound"
         base_url: "http://your-vllm-host:8000/v1"
     tiers:
       cheap: ["qwenc"]
       cheap_builder: goose
   ```
5. **Judge note.** `judge.policy: advisory` with abe not installed proceeds to
   apply/candidate (`JudgeUnavailable → Apply`) and logs a `judge-unavailable`
   line in the iteration artifacts — that line is advice, not the stop reason.

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
