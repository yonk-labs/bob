# Bob Agent Lifecycle Spec

## TLDR

Bob should stay a narrow executor: one bounded build -> verify -> judge loop over an isolated worktree. The next product step is not "make Bob a whole agent framework"; it is to make Bob a stable worker primitive that an orchestrator can call repeatedly with per-run gates, clear scope, structured results, cleanup, model fallback, and an optional policy that feeds Abe critique back into the builder.

The larger lifecycle should be separate, thin agents around Bob: Hector writes executable specs/tests and decomposes work, Bob implements one verified slice, Abe judges, Greta reviews UX/design, and a frontier orchestrator owns product intent, sequencing, and final judgment.

## Problem

The current product works for the strongest discovered pattern:

1. A human or frontier model writes a precise failing test.
2. The allowed implementation paths are frozen.
3. Bob runs opencode in an isolated worktree.
4. Bob iterates until the objective gate passes.
5. Abe provides critique after the gate.

That is enough for cheap local models to land small mechanical changes. The rough edges are around the loop:

- Abe can catch real problems, but Bob currently treats Abe as non-blocking prose.
- Per-task verify and scope settings require editing `bob.yaml`.
- Failed runs leave worktrees/branches that can break later test runners.
- Multi-file work stalls because Bob receives one task, not a decomposed queue.
- Builder failures hide the useful stderr/context.
- Model fallback is manual.
- MCP output is too small for an orchestrator to decide the next action without reading logs.

## Product Boundary

Bob is the executor, not the whole lifecycle.

Bob owns:

- isolated worktree creation and cleanup
- builder invocation
- objective verification
- scope and secret guards
- judge invocation
- retry decisions inside one bounded task
- structured result reporting
- optional apply/commit of a converged candidate

Bob does not own:

- product strategy
- architecture decisions
- UX design
- test/spec authoring for broad features
- backlog decomposition
- arbitrary multi-agent scheduling
- writing a replacement for opencode

That boundary keeps Bob boring enough to trust.

## Current Architecture Evidence

From the current repo:

- `src/engine.rs` already has a pure `next_action` decision point, which is the right place to add judge policy.
- `src/builder.rs` correctly shells out to `opencode run --dir <worktree>`, which is required for worktree isolation.
- `src/judge.rs` can parse explicit `verdict` values, but current Abe `validate` output often degrades to `Verdict::Uncertain` plus prose.
- `src/mcp.rs` exposes only `task`, `spec`, `files`, `max_iters`, `apply`, and `model`.
- `src/report.rs` returns `status`, `base_sha`, `iterations`, `applied`, `stop_reason`, and `final_diff`, but not verify/judge details.
- `src/worktree.rs` now has first-class `bob gc`; default run cleanup should keep artifacts while avoiding stale worktrees.

## Design Decisions

### 1. Keep opencode as the default builder

Decision: keep opencode under Bob for v1/v2.

Why:

- Bob should not become a coding copilot.
- opencode already owns provider/model integration, project editing, and tool use.
- Bob's value is the loop around a builder: isolation, gates, retry, scope, judge, and reporting.
- Replacing opencode would expand Bob into a much bigger, easier-to-break product.

Required change: define a minimal builder adapter contract so opencode is the first adapter, not an unchangeable assumption.

```text
BuilderAdapter:
  input:
    workdir
    prompt
    model
    timeout
    extra_args
  output:
    exit_status
    stdout_tail
    stderr_tail
    failure_kind
```

Supported `failure_kind` values:

- `ok`
- `timeout`
- `auth`
- `rate_limit`
- `context_overflow`
- `no_diff`
- `crash`
- `unknown`

Bob should still capture the diff itself from git. The builder adapter reports execution health, not code correctness.

### 2. Add judge policy

Current behavior is effectively:

```yaml
judge:
  policy: advisory
```

Add:

```yaml
judge:
  cmd: abe
  mode: validate
  timeout_secs: 600
  policy: advisory # advisory | blocking | retry_on_fail
```

Policies:

- `advisory`: current behavior. Verify pass means convergence; Abe critique is reported only.
- `blocking`: verify must pass and Abe must return `pass`. `fail` or `uncertain` stops the run.
- `retry_on_fail`: verify passes, then Abe returns `fail` or useful `uncertain` critique, and Bob feeds that critique back into the builder until the loop bound is hit.

Default stays `advisory` for compatibility. Autonomous orchestrators should prefer `retry_on_fail`.

`retry_on_fail` must not loop forever on vague prose. If Abe returns `uncertain` with no actionable critique, Bob should converge under verify and report the uncertainty.

### 3. Per-run overrides are mandatory

`bob.yaml` is project policy. A delegated slice needs task policy.

Add CLI/MCP overrides:

```text
verify_cmds
allow_paths
max_changed_files
max_changed_lines
judge_policy
model
model_fallbacks
apply
auto_commit
keep_artifacts
keep_worktree
```

These overrides never mutate `bob.yaml`.

Example MCP call:

```json
{
  "task": "Implement GET /api/roster-plan so the focused test passes.",
  "files": ["src/routes/api/roster-plan.js"],
  "verify_cmds": ["npx jest tests/routes/roster-plan.test.js"],
  "allow_paths": ["src/routes/api/"],
  "judge_policy": "retry_on_fail",
  "model": "qwen-193",
  "apply": false
}
```

### 4. Return structured run state

Extend `RunResult` so the caller never scrapes logs.

```json
{
  "status": "converged",
  "next_action": "review_candidate",
  "base_sha": "abc123",
  "run_id": "mcp-123-0",
  "worktree": ".bob/worktrees/mcp-123-0",
  "artifact_dir": ".bob/runs/mcp-123-0",
  "iterations": 2,
  "applied": false,
  "committed": false,
  "changed_files": ["src/routes/api/roster-plan.js"],
  "scope": {
    "within": true,
    "files": 1,
    "lines": 72,
    "detail": "1 files, 72 lines"
  },
  "verify": {
    "passed": true,
    "cmd": "npx jest tests/routes/roster-plan.test.js",
    "output_tail": "PASS tests/routes/roster-plan.test.js"
  },
  "judge": {
    "policy": "retry_on_fail",
    "verdict": "pass",
    "critique": ""
  },
  "builder": {
    "model": "qwen-193",
    "fallbacks_tried": [],
    "stderr_tail": ""
  },
  "stop_reason": null,
  "final_diff": "..."
}
```

`next_action` values:

- `done`
- `review_candidate`
- `apply_candidate`
- `retry_with_judge_critique`
- `retry_with_verify_failure`
- `clean_stale_worktrees`
- `split_task`
- `escalate_model`
- `human_decision_required`

### 5. Add cleanup as a product feature

Add:

```text
bob gc
```

Behavior:

- remove stale `.bob/worktrees/*`
- remove matching `bob/*` branches whose worktree is gone
- run `git worktree prune`
- print what was removed
- support `--dry-run`

Also:

- Split `keep_artifacts` from `keep_worktree`.
- Default to preserving artifacts and cleaning worktrees on non-converged runs unless `keep_worktree=true`.
- When a JS/TS repo is detected, warn once if `.bob/` is not ignored by the test runner/gitignore.

### 6. Add model fallback without a scheduler

Bob does not need a general agent scheduler to try a better model.

Add:

```yaml
builder:
  model: qwen-193
  fallback_models:
    - stronger-coder
```

Fallback triggers:

- builder infra failure
- timeout
- context overflow
- `EmptyDiffAfterCritique`
- repeated identical verify failure if `--fallback-on-stuck` is set

Bob should log the reason:

```text
qwen-193 stalled with EmptyDiffAfterCritique; retrying with stronger-coder
```

### 7. Carry lessons forward, but keep them explicit

Agents should receive project lessons learned from prior runs, but only as a small curated ledger. Do not rely on hidden conversation memory, and do not dump old logs into every prompt.

Add a repo-local file:

```text
.bob/lessons.md
```

Format:

```markdown
# Bob Lessons

## Project Rules

- Do not run two `npm test` processes at once; this repo flakes under CPU oversubscription.
- Add `.bob/` to Jest/module-map ignores.

## Patterns That Worked

- Write the failing focused test first, commit it, then restrict Bob with `allow_paths`.
- Use single-file or one-file-plus-mount slices for local coder models.

## Failure Patterns

- Multi-file domain+route+export tasks tend to stall local qwen; split them first.
- If Abe flags a real issue after verify passes, rerun with `judge_policy=retry_on_fail`.
```

Rules:

- Keep lessons factual, short, and project-specific.
- Prefer "when X, do Y" over vague advice.
- Remove stale lessons when the codebase changes.
- Never store secrets, private customer data, or full terminal logs.
- Bob may include `.bob/lessons.md` in builder and judge prompts when present.
- Hector should update the ledger after a campaign if a failure pattern repeats.

This is the anti-repeat mechanism: the orchestrator and subagents get prior operational knowledge without pretending every old run is equally relevant.

## Lifecycle Agents

These agents should be separate call patterns/tools, not merged into Bob core.

### Agent Packaging

Hector and Greta need their own specs before they need their own repos.

Keep them in this repo at first:

```text
docs/agents/hector.md
docs/agents/greta.md
```

Each spec should define:

- role
- inputs
- outputs
- refusal boundaries
- handoff format to Bob/Abe/orchestrator
- acceptance checks
- example task packet

Do not create separate repos until an agent has its own executable code, release cycle, config, tests, or external users. Until then, separate repos add coordination cost without buying isolation.

### Orchestrator

Model: frontier model.

Owns:

- user intent
- architecture direction
- task decomposition approval
- deciding which agent runs next
- final review and merge decision

The orchestrator should call Bob, not become Bob.

### Hector

Role: TDD/spec agent.

Owns:

- converting a feature request into executable tests
- writing the smallest focused failing test per slice
- defining `verify_cmds`
- defining `allow_paths`
- splitting multi-file work into Bob-sized tasks
- detecting when a task is not Bob-shaped

Hector should not be the multi-file implementer. Hector makes multi-file work tractable by producing a queue of small verified slices.

Hector output:

```json
{
  "campaign": "roster-plan-api",
  "slices": [
    {
      "name": "summary endpoint",
      "test_files": ["tests/routes/roster-plan-summary.test.js"],
      "verify_cmds": ["npx jest tests/routes/roster-plan-summary.test.js"],
      "allow_paths": ["src/routes/api/", "src/app.js"],
      "task": "Implement GET /api/roster-plan so the summary endpoint test passes.",
      "expected_complexity": "single_file_plus_mount"
    }
  ],
  "human_questions": []
}
```

### Bob

Role: implementation worker.

Owns one slice:

- build
- verify
- judge
- retry
- return structured result

Bob should reject or return `next_action: split_task` when the task is too broad for the configured caps.

### Abe

Role: critic/judge.

Owns:

- adversarial review of diff against spec
- risk identification
- structured verdict when available

Bob should be able to use Abe in either advisory or retry/blocking mode.

### Greta

Role: UX/design agent.

Owns:

- user journey critique
- UI copy and flow review
- accessibility basics
- browser journey acceptance criteria
- design acceptance tests where possible

Greta should run before Hector for UX-heavy work and after Bob for browser/user-flow review. Greta should not gate low-level mechanical tasks unless the task is user-facing.

### Senior Reviewer

Model: frontier model.

Owns:

- architecture review
- security review for risky changes
- final PR review
- deciding whether a campaign should continue

This can be Abe with stronger prompts/models, or a separate role. Do not put this inside Bob.

## Multi-file Work Strategy

Bob should support multi-file diffs, but not broad ambiguous tasks.

Use this ladder:

1. Single file with focused gate: Bob can own it.
2. Two to three files with obvious wiring and focused gate: Bob can own it with `allow_paths`.
3. Multi-file feature with unclear tests: Hector decomposes first.
4. Architecture or UX decision: Orchestrator/Greta decides first.
5. Large refactor: frontier model creates a plan, Hector creates gates, Bob drains slices.

The "campaign runner" is the right abstraction for multi-file autonomy.

## Campaign Runner

Add after core Bob stability.

Input:

```json
{
  "name": "roster-plan-api",
  "base_branch": "feature/roster-plan",
  "auto_apply": true,
  "auto_commit": true,
  "max_slices": 5,
  "slices": [
    {
      "task": "...",
      "verify_cmds": ["..."],
      "allow_paths": ["..."],
      "model": "qwen-193",
      "judge_policy": "retry_on_fail"
    }
  ]
}
```

Loop:

1. Confirm focused gate is red when a test slice is expected to start red.
2. Call Bob for the slice.
3. If converged, apply.
4. Re-run focused gate.
5. Commit the slice if `auto_commit=true`.
6. Stop on ambiguity, scope breach, repeated infra failure, or human question.

Campaign runner output is a campaign report plus a branch/PR, not a giant patch.

## CLI/API Changes

### CLI

```text
bob build <task>
  [--spec FILE]
  [--files PATH ...]
  [--verify CMD ...]
  [--allow-path PATH ...]
  [--max-changed-files N]
  [--max-changed-lines N]
  [--judge-policy advisory|blocking|retry_on_fail]
  [--model NAME_OR_ID]
  [--fallback-model NAME_OR_ID ...]
  [--apply]
  [--auto-commit]
  [--keep-worktree]

bob gc [--dry-run]
bob campaign --file campaign.json
```

### MCP `build`

Add fields matching the CLI overrides:

```json
{
  "task": "string",
  "spec": "string?",
  "files": ["string"],
  "verify_cmds": ["string"],
  "allow_paths": ["string"],
  "max_changed_files": "number?",
  "max_changed_lines": "number?",
  "judge_policy": "advisory|blocking|retry_on_fail",
  "model": "string?",
  "fallback_models": ["string"],
  "apply": "boolean?",
  "auto_commit": "boolean?",
  "keep_worktree": "boolean?"
}
```

MCP `apply` continues to default to `false`.

## Implementation Plan

### Phase 1: Make one Bob run autonomous enough

1. Add `JudgePolicy` to config, CLI, and MCP.
2. Change `engine.rs` so judge failure can continue the loop under `retry_on_fail`.
3. Extend `RunResult` with verify, judge, scope, builder, artifact, worktree, changed file, and `next_action` fields.
4. Add per-run `verify_cmds` and scope overrides.
5. Capture builder stdout/stderr tails and classify obvious failures.
6. Add tests around `next_action`.

Done when an orchestrator can call Bob once and know exactly whether to review, retry, split, clean, or escalate.

### Phase 2: Remove sharp edges

1. Add `bob gc --dry-run`.
2. Split `keep_artifacts` from `keep_worktree`.
3. Clean worktrees by default after non-converged runs once artifacts are written.
4. Add better `.git` permission and worktree diagnostics.
5. Warn on likely Jest/Haste collisions when `.bob/` is not ignored.

Done when failed runs do not poison the next run.

### Phase 3: Add cheap model resilience

1. Add `fallback_models`.
2. Retry on infra failure/stall with the next configured model.
3. Include fallback history in `RunResult`.
4. Keep the fallback chain bounded by walltime and max attempts.

Done when local small models remain the default and stronger models are used only on clear failure modes.

### Phase 4: Add campaign runner

1. Define campaign JSON.
2. Run slices serially.
3. Support auto-apply and auto-commit only after green gate, scope pass, and secret scan.
4. Stop on human questions or `next_action: split_task`.
5. Produce campaign report.

Done when Hector can hand Bob a queue and Bob can drain it without hand-editing `bob.yaml`.

## Required Tests

Small, focused tests only.

- `advisory`: verify pass + Abe fail still converges and reports critique.
- `blocking`: verify pass + Abe fail does not converge.
- `retry_on_fail`: verify pass + Abe fail feeds critique into next builder prompt.
- `retry_on_fail`: vague uncertain judge output does not loop forever.
- per-run `verify_cmds` overrides config without mutating `bob.yaml`.
- per-run `allow_paths` overrides config without mutating `bob.yaml`.
- MCP result includes `next_action`, `verify`, `judge`, `scope`, and `changed_files`.
- `bob gc --dry-run` reports stale worktrees/branches without deleting them.
- fallback model records original failure and successful fallback.

## Non-goals

- No custom coding copilot.
- No general multi-agent framework in Bob core.
- No concurrent campaign execution yet.
- No automatic merge to `main`.
- No generated-test gaming defense beyond scope caps and frozen paths in this pass.
- No container sandbox in this pass.

## Open Questions

1. Should `blocking` treat Abe `uncertain` as failure or human-review-required? Default recommendation: human-review-required.
2. Should campaign runner live in Bob or in a separate tool that calls Bob over MCP? Default recommendation: start in Bob only if it stays a thin serial runner.
3. Should `auto_commit` create one commit per slice or one campaign commit? Default recommendation: one commit per slice for easier revert/review.
4. Should Hector be a named external agent or a Bob command like `bob plan-tests`? Default recommendation: keep Hector external until its interface is stable.
