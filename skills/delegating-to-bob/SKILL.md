---
name: delegating-to-bob
description: This skill should be used when the user asks to "have bob build X", "delegate this to bob", "use bob to implement/fix", "auto-fix the failing tests", "build and verify this", "pass this work to bob", "offload this build", or when a concrete implementation task can be objectively verified (its tests/build must pass) and should be done in an isolated worktree, verified, and optionally applied. Guides delegating coding work to bob's build→verify→judge loop (via the `build` MCP tool or `bob build`).
version: 0.1.0
---

# Delegating build work to bob

bob is an autonomous **build → verify → judge** loop. Hand it an implementation task and it drives a coding CLI (`opencode`) to make the change in an **isolated git worktree**, runs the project's **objective verify gate** (e.g. `cargo test`, `npm test`), and converges only when that gate passes — then returns a verified diff, or applies it. Delegating to bob turns "write this code" into "write this code *and prove the tests pass before it touches the tree*."

## When to delegate to bob

Delegate when ALL of these hold:
- The task is a concrete **implementation or fix** (not open-ended design, research, or discussion).
- "Done" can be checked by an **objective command** — a test suite, a build, a lint, or a script.
- Isolation and verification are wanted: the change should be proven correct before it lands.

Do not delegate to bob for architectural decisions, research, code review, or anything lacking an objective pass/fail gate. Do that work directly, or use a debate/validation tool.

## Check setup first

bob needs `bob` on PATH plus `git`, `opencode`, and `abe`, and a `bob.yaml` in the target repo.
- Run `bob doctor` — it reports any missing piece.
- If there is no `bob.yaml`, run `bob init`, then set `verify.cmds` to the project's test/build command. This gate is what bob converges on — setting it correctly is the single most important step.

## How to call bob

Prefer the **`build` MCP tool** (exposed by this plugin). Parameters:
- `task` (required) — the implementation instruction; state precisely what "done" means.
- `spec` (optional) — fuller spec text when one is referenced.
- `files` (optional) — relevant file paths to mention in the prompt.
- `max_iters` (optional) — override the loop cap.
- `apply` (optional) — set `true` to land the change on the user's tree on convergence; omit (the default) to **propose** — bob returns a verified candidate diff with the tree untouched. Default to propose unless the user explicitly asks to apply.

Alternatively use the `/bob:build <task>` command, or the shell: `bob build "<task>" [--apply]` (exit 0 = converged, non-zero = did not converge).

## After bob returns

bob returns a `RunResult`: `status` (`converged` / `not_converged` / `error`), `applied`, `iterations`, `stop_reason`, and `final_diff`. Always:
- On **converged**: summarize what changed (from `final_diff`); if `applied` is false, the diff is a proposal the user can choose to apply.
- On **not_converged**: surface `stop_reason` and the last `final_diff`. Common reasons: the builder produced no change (stuck), the verify gate kept failing, or the diff exceeded scope caps. Do not silently retry — report what bob got stuck on so the user can decide.

## Phrase the task well

bob's builder sees only `task`/`spec`, not the surrounding conversation. Write a clear implementation instruction with an explicit success condition — "Implement `parse_config` in src/config.rs so the existing tests pass" beats "improve config handling". Name the key files in `files`.

## Tuning knobs (in the project's `bob.yaml`)

- **Choose the builder's model:** `builder.args: ["--model", "provider/model"]` (forwarded to opencode).
- **Guardrails:** `verify.cmds` is an extensible gate — add lints, scanners, or policy scripts (all must pass). `scope.allow_paths: ["src/"]` restricts which paths may change; `scope.max_changed_files` / `max_changed_lines` cap the blast radius.
- **Cost/time:** `loop.max_iterations`, `loop.max_walltime_secs`.

bob isolates every attempt in a worktree and secret-scans the spec and the diff, so delegation is safe by default: nothing lands until the gate passes and (for `apply`) the user opted in.
