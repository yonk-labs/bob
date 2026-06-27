# Bob Autonomy Spec

This is the overlord assessment from using bob as a subagent in this repo. The core product works: bob can run an isolated build loop, apply a verified diff, and surface abe's critique. The rough edges are in orchestration, chaining, and turning judge feedback into autonomous action.

## Goal

Make bob a better autonomous worker for a supervising agent:

- The overlord supplies intent, constraints, and final judgment.
- Bob owns implementation, verification, cleanup, and retry mechanics.
- Abe critique can become loop input when configured, not just prose for the overlord to manually re-feed.
- The MCP/CLI result is structured enough for an orchestrator to decide the next action without scraping logs.

## Current Friction

1. **Abe is advisory-only.**
   Today bob can converge on `cargo test` while abe still finds a real issue. That forced the overlord to start new bob runs manually with the critique pasted back in.

2. **Applied diffs are staged in a way that makes chaining awkward.**
   Bob applied successful work into the index. To run a follow-up bob pass against that staged state, the overlord had to create temporary commit objects and extra worktrees.

3. **Run state is too log-shaped.**
   The useful facts exist in output, but a supervising agent needs structured fields: verifier output, judge critique, whether critique was blocking, applied paths, branch/worktree path, and suggested next action.

4. **Worktree cleanup and Git permissions are sharp edges.**
   Bob needs `.git` write access for branches/worktrees. When that fails, the error is real but not actionable enough. Failed or interrupted nested runs can also leave worktrees that future runs trip over.

5. **Per-run config is too static.**
   `bob.yaml` is good project policy, but overlords often need per-task overrides for `verify.cmds`, `scope.allow_paths`, model choice, apply mode, and judge strictness.

## Proposed Behavior

### 1. Add Judge Policy

Add a config field and CLI/MCP override:

```yaml
judge:
  cmd: abe
  mode: validate
  timeout_secs: 600
  policy: advisory # advisory | blocking | retry_on_fail
```

- `advisory`: current behavior.
- `blocking`: verify must pass and abe must pass.
- `retry_on_fail`: verify passes, abe fails, bob feeds abe critique back to the builder and continues until max iterations.

Default should stay `advisory` for backward compatibility. Overlords should use `retry_on_fail` for autonomous subagent work.

### 2. Return Structured Follow-Up State

Extend `RunResult` JSON with fields like:

```json
{
  "status": "converged",
  "verify_passed": true,
  "judge_policy": "retry_on_fail",
  "judge_verdict": "fail",
  "judge_critique": "...",
  "next_action": "retry_with_judge_critique",
  "worktree": ".bob/worktrees/...",
  "changed_files": ["src/init.rs"]
}
```

The overlord should never need to parse terminal logs to know what happened.

### 3. Make Chaining First-Class

Add a way to continue from the current dirty/staged candidate state:

- `bob build --base working-tree`
- `bob build --base index`
- MCP param: `base: "head" | "index" | "working_tree"`

This avoids temporary stash commits when a supervisor wants bob pass 2 to refine bob pass 1 before a human commit.

### 4. Add Per-Run Overrides

Expose common task-local fields in CLI and MCP:

- `verify_cmds`
- `allow_paths`
- `max_changed_files`
- `max_changed_lines`
- `judge_policy`
- `model`
- `apply`

`bob.yaml` remains the default policy. Per-run overrides let an overlord freeze scope without editing repo config.

### 5. Improve Cleanup

Add:

- `bob gc` to remove stale `.bob/worktrees/*` and matching `bob/*` branches.
- Automatic cleanup on normal convergence.
- Clear diagnostics when `.git` is read-only or worktree creation fails.
- A `keep_artifacts`/`keep_worktree` split so logs can remain while dead worktrees go away.

### 6. Installer Lessons

The installer should keep these rules:

- Build `bob.yaml` by serializing config structs, not string-concatenating YAML.
- Detect commands by PATH filesystem lookup, not by executing `--version`.
- Do not vendor opencode. Prefer existing PATH, then show official install:
  `curl -fsSL https://opencode.ai/install | bash`
- Treat abe as optional for build convergence unless judge policy says otherwise.

## Minimal Implementation Plan

1. Add `JudgePolicy` to config and CLI/MCP overrides.
2. Change engine decision logic so judge failure can become the next builder prompt when policy is `retry_on_fail`.
3. Extend `RunResult` with structured verify/judge/follow-up fields.
4. Add per-run `verify_cmds` and `allow_paths`.
5. Add `bob gc`.
6. Add focused tests for:
   - verify pass + judge fail retries under `retry_on_fail`
   - verify pass + judge fail converges under `advisory`
   - MCP result includes `judge_critique` and `next_action`
   - per-run overrides do not mutate `bob.yaml`

## Non-Goals

- No container sandbox in this pass.
- No multi-agent scheduler.
- No custom opencode installer beyond detection and clear install instructions.
- No new dependency unless stdlib becomes meaningfully worse than the alternative.
