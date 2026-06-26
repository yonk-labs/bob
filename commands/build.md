---
description: Delegate an implementation task to bob — it builds the change in an isolated worktree, gates on your tests, and returns a verified diff (or applies it)
---

Use the `bob` `build` MCP tool to delegate the implementation task below. bob drives a coding CLI in an isolated git worktree, runs the project's verify gate (e.g. its tests), and converges only when that gate passes.

First confirm setup: the project needs a `bob.yaml` whose `verify.cmds` is its test/build command. Run `bob doctor`; if there's no config, run `bob init` and set `verify.cmds`.

Call the `build` tool with:
- **`task`** — the implementation instruction (the text below). Be concrete about what "done" means.
- **`spec`** (optional) — fuller spec text if one is referenced.
- **`files`** (optional) — relevant file paths to mention.
- **`max_iters`** (optional) — override the iteration cap.
- **`apply`** (optional) — set `true` ONLY if the user wants the change landed in their working tree. Omit (the default) to *propose*: bob returns a verified candidate diff without touching files.

When it returns, report the `status` (converged / not_converged), whether it `applied`, the `stop_reason` if it didn't converge, and a summary of `final_diff`. If it did not converge, surface the diff + reason — do not silently retry.

If the `bob` MCP tool is unavailable, fall back to the shell: `bob build "$ARGUMENTS" [--apply] [--max-iters N]` (exit 0 = converged, non-zero = did not).

Task: $ARGUMENTS
