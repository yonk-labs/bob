# Field report: the full Route loop, live, on a 3B local model

**Date:** 2026-07-05 · **Setup:** macOS, ollama serving `llama3.2:3b` (2 GB model), workspace-built binaries, zero cloud calls.

## What ran

A clean toy repo (Python, pytest) and one task: *"Add clamp(x, lo, hi) to src/clamp.py per spec."* Every stage below used `llama3.2:3b` — the whole loop consumed **zero frontier tokens**.

```
hector plan  → llama wrote the test, hector proved it RED, froze campaign.yaml
bob campaign → llama (ThinBuilder, curl) implemented clamp
             → verify gate: pytest GREEN in the worktree, replay-verified
             → scope clean → applied + committed to the repo
result       → status: converged | applied: true | committed: true
             → main tree: pytest 1 passed
```

`route mcp` was separately verified serving all seven tools (`debate`, `validate`, `build`, `plan`, `review`, `bundle`, `enumerate`) over stdio — the overlord surface a frontier model drives.

An `abe debate` (two llama panelists + chairman, synthesis protocol) ran live on the same endpoint and reported its own accounting: `usage: 5 calls, ~1,681 est tokens in, ~1,238 est tokens out`.

## What the gates caught (live, same session)

The run wasn't clean on the first try — which is the point. Each failure was caught by a gate, not merged:

1. **Path-mismatch false red.** The model wrote `clamp_test.py` at the repo root but its `verify_cmd` said `pytest src/clamp_test.py` — exit 4, zero tests executed, and (pre-fix) the probe froze a gate that never ran a test. hector now classifies the-test-never-ran shapes as infrastructure errors, feeds the mismatch back, and the model corrected itself on the next attempt.
2. **Uncollectible test.** The model wrote module-level asserts — import-crash red at plan time, but `pytest` exits 5 ("no tests ran") forever after implementation. hector now rejects pytest tests with no `def test_` and teaches the collectible shape; attempt two produced a proper test.
3. **Trailing prose in generated files.** bob's engine prompt invites end-of-response notes; the thin builder wrote the model's `### CONCERNS ###` paragraph *into* `clamp.py` (syntax error every iteration). The format now ends with `=== END ===` and the parser stops there.
4. **Scope envelope.** pytest's `__pycache__/*.pyc` byproducts landed outside `editable_paths` → `ScopeExceeded`, `applied: false`. Junk never reached the tree. (Lesson for Python repos: gitignore `__pycache__/` before running campaigns.)
5. **Frozen tests.** The model modified the test file mid-build; bob reset it (`resetting 1 test file(s) the model modified`). A builder can't grade its own homework.
6. **Advisory judge degradation.** A stale `abe` 0.4.1 on PATH predated `--verdict`; the judge call failed, was recorded as advisory, and the run proceeded on the objective gate alone. Refresh installed binaries with `cargo install --path abe`.

## The cost shape

The frontier model's role in this workflow is the two ends: writing the task/spec and judging the result. Everything between — test writing, retries, implementation, verify loops — ran on hardware whose marginal cost is electricity. In this session that middle was: 3 planner model calls (two rejected by gates, one frozen), 1 builder call, plus verify runs. All local. The abe usage block (`calls / est_tokens_in / est_tokens_out` on every surface) is what lets you put a number on the frontier-side spend of a judged handoff instead of guessing.

## Reproduce

```sh
ollama pull llama3.2:3b
cargo build --workspace
# toy repo: pyproject.toml + spec.md + git init
hector plan --task "..." --spec spec.md --editable-path src/clamp.py --out campaign.yaml
bob campaign --file campaign.yaml   # bob.yaml: builder.cmd: thin, model → localhost:11434/v1
```
