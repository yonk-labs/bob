---
description: Delegate an implementation task to bob — it builds the change in an isolated worktree, gates on your tests, and returns a verified diff (or applies it)
argument-hint: "<implementation task>"
---

Use the `bob` `build` MCP tool to delegate the implementation task below. bob drives a coding CLI in an isolated git worktree, runs the project's verify gate (e.g. its tests), then applies the configured judge policy.

First confirm setup: the project needs a `bob.yaml` whose `verify.cmds` is its test/build command. Run `bob doctor`; if there's no config, run `bob init` and set `verify.cmds`.

Call the `build` tool with:
- **`task`** — the implementation instruction (the text below). Be concrete about what "done" means.
- **`spec`** (optional) — fuller spec text if one is referenced.
- **`files`** (optional) — relevant file paths to mention.
- **`max_iters`** (optional) — override the iteration cap.
- **`verify_cmds`** (optional) — focused gate commands for this task.
- **`allow_paths`** (optional) — path prefixes Bob may edit.
- **`judge_policy`** (optional) — use `retry_on_fail` when Abe critique should be fed back into Bob.
- **`fallback_models`** (optional) — ordered fallback models if the builder errors or stalls.
- **`apply`** (optional) — set `true` ONLY if the user wants the change landed in their working tree. Omit (the default) to *propose*: bob returns a verified candidate diff without touching files.

When it returns, report the `status` (converged / not_converged), `next_action`, whether it `applied`, the `stop_reason` if it didn't converge, and a summary of `final_diff`. If it did not converge, surface the diff + reason — do not silently retry.

If the `bob` MCP tool is unavailable, fall back to the shell: `bob build "$ARGUMENTS" [--apply] [--max-iters N] [--verify CMD] [--allow-path PATH] [--judge-policy retry_on_fail] [--fallback-model NAME]` (exit 0 = converged, non-zero = did not).

Task: $ARGUMENTS
