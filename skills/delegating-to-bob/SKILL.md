---
name: delegating-to-bob
description: This skill should be used when the user asks to "have bob build X", "delegate this to bob", "use bob to implement/fix", "auto-fix the failing tests", "build and verify this", "pass this work to bob", "offload this build", or when a concrete implementation task can be objectively verified (its tests/build must pass) and should be done in an isolated worktree, verified, and optionally applied. Guides delegating coding work to bob's build→verify→judge loop (via the `build` MCP tool or `bob build`).
version: 0.1.0
---

# Delegating build work to bob

bob is an autonomous **build → verify → judge** loop. Hand it an implementation task and it drives a builder CLI — **`goose`** (the agent-loop builder used for the cheap/medium/large tiers) or **`opencode`** (the heavier frontier-tier backend) — to make the change in an **isolated git worktree**, runs the project's **objective verify gate** (e.g. `cargo test`, `npm test`), then applies `judge.policy` (`advisory`, `blocking`, or `retry_on_fail`) before returning a verified diff or applying it. Which builder runs is decided by the tier config in `bob.yaml` (`*_builder` keys); set `cmd: goose` for a tier-less config to force goose. Delegating to bob turns "write this code" into "write this code *and prove the tests pass before it touches the tree*."

## When to delegate to bob

Delegate when ALL of these hold:
- The task is a concrete **implementation or fix** (not open-ended design, research, or discussion).
- "Done" can be checked by an **objective command** — a test suite, a build, a lint, or a script.
- Isolation and verification are wanted: the change should be proven correct before it lands.

Do not delegate to bob for architectural decisions, research, code review, or anything lacking an objective pass/fail gate. Do that work directly, or use a debate/validation tool.

## Check setup first

bob needs `bob` on PATH plus `git`, a builder CLI (`goose` for the cheap/medium/large tiers, `opencode` for the frontier tier), and `abe`, and a `bob.yaml` in the target repo.
- Run `bob doctor` — it reports any missing piece.
- If there is no `bob.yaml`, run `bob init`, then set `verify.cmds` to the project's test/build command. This gate is what bob converges on — setting it correctly is the single most important step.

## How to call bob

Prefer the **`build` MCP tool** (exposed by this plugin). Parameters:
- `task` (required) — the implementation instruction; state precisely what "done" means.
- `spec` (optional) — fuller spec text when one is referenced.
- `files` (optional) — relevant file paths to mention in the prompt.
- `max_iters` (optional) — override the loop cap.
- `verify_cmds` (optional) — override verify gates for this run.
- `allow_paths` (optional) — restrict which path prefixes this run may change.
- `max_changed_files` / `max_changed_lines` (optional) — override scope caps.
- `judge_policy` (optional) — `advisory`, `blocking`, or `retry_on_fail`; use
  `retry_on_fail` when Abe critique should be fed back into Bob autonomously.
- `model` (optional) — builder model name from `builder.models`, or raw provider/model id.
- `fallback_models` (optional) — ordered fallback models to try if the builder errors or stalls.
- `keep_worktree` (optional) — keep the worktree for debugging; artifacts are always kept.
- `apply` (optional) — set `true` to land the change on the user's tree on convergence; omit (the default) to **propose** — bob returns a verified candidate diff with the tree untouched. Default to propose unless the user explicitly asks to apply.

Alternatively use the `/bob:build <task>` command, or the shell: `bob build "<task>" [--apply]` (exit 0 = converged, non-zero = did not converge).

Use `bob campaign --file campaign.yaml` only when you already have a serial list of Bob-sized slices. Multi-slice campaigns require `auto_commit: true` and a clean working tree; Bob creates one commit per converged slice.

## After bob returns

bob returns a `RunResult`: `status` (`converged` / `not_converged` / `error`), `next_action`, `applied`, `iterations`, `stop_reason`, `changed_files`, `verify`, `judge`, `scope`, `builder.fallbacks_tried`, and `final_diff`. Always:
- On **converged**: summarize what changed (from `final_diff`); if `applied` is false, the diff is a proposal the user can choose to apply.
- On **not_converged**: surface `stop_reason` and the last `final_diff`. Common reasons: the builder produced no change (stuck), the verify gate kept failing, or the diff exceeded scope caps. Do not silently retry — report what bob got stuck on so the user can decide.
- If stale `.bob/worktrees/*` directories or `bob/*` branches interfere with later tooling, run `bob gc --dry-run`, then `bob gc`.

## Phrase the task well

bob's builder sees only `task`/`spec`, not the surrounding conversation. Write a clear implementation instruction with an explicit success condition — "Implement `parse_config` in src/config.rs so the existing tests pass" beats "improve config handling". Name the key files in `files`.

If `.bob/lessons.md` exists, bob includes it in builder and judge context. Keep lessons short, factual, and project-specific; do not paste logs or secrets.

## Tuning knobs (in the project's `bob.yaml`)

- **Choose the builder's model:** keep a named roster in `builder.models` and per-tier model lists in `builder.tiers`; set `builder.model` for the default. (Tier-less configs can still pass `builder.args: ["--model", "provider/model"]` to opencode.)
- **Guardrails:** `verify.cmds` is an extensible gate — add lints, scanners, or policy scripts (all must pass). `scope.allow_paths: ["src/"]` restricts which paths may change; `scope.max_changed_files` / `max_changed_lines` cap the blast radius. Override these per run when handing Bob a frozen slice.
- **Judge policy:** `judge.policy: advisory` keeps Abe non-blocking; `blocking` requires Abe to pass; `retry_on_fail` feeds Abe critique back into the next builder prompt.
- **Model fallback:** per-run `fallback_models` retries on builder errors and clear stuck results; `builder.escalation_policy: none` keeps a run in its slice's tier (no escalation into paid tiers).
- **Cost/time:** `loop.max_iterations`, `loop.max_walltime_secs`.

bob isolates every attempt in a worktree and secret-scans the spec and the diff, so delegation is safe by default: nothing lands until the gate passes and (for `apply`) the user opted in.
