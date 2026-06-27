# Hector/Bob/Abe Lifecycle Field Report

Date: 2026-06-27

## Work Completed

- Built Hector to feature-complete MVP:
  - `hector plan`
  - `hector check`
  - `hector review`
  - `hector frontier-brief`
  - `hector mcp`
  - `skills/hector-frontier/SKILL.md`
- Updated Bob docs and comments to describe the real lifecycle:
  - frontier orchestrator decides product behavior
  - Greta settles UX when needed
  - Hector writes proof-driven slices
  - Bob implements bounded slices
  - Abe reviews or blocks according to policy
- Added tests for Hector plan/check/review/brief/MCP wrappers.
- Verified:
  - Hector: `cargo test` passed, 20 tests.
  - Bob: `cargo test` passed, 56 unit tests + 1 integration test.

## Findings

Hector should own slice design, not implementation. Its useful output is a Bob campaign with exact behavior, verify command, editable paths, reference paths, scope caps, and review policy.

Bob is strongest as a narrow worker. It should receive one bounded behavior at a time. Local/smaller models did well on tight single-file tasks and struggled when the slice mixed domain logic, exports, route wiring, and product interpretation.

Bob must not be allowed to edit the proof. Tests/specs belong in `reference_paths`; production code belongs in `editable_paths`.

`needs_review` is not failure and not success. It means the verify gate passed but Abe or the judge path did not provide enough confidence. The next owner is the frontier orchestrator or human reviewer.

Fallback models should be escalation only. They are useful for builder stalls or infra errors, not as a substitute for splitting vague work.

## Abe Judge/Moderator Finding

Abe does have decisive judge functionality:

- `abe debate --protocol judge ...` uses the configured chairman/judge model to score answers and pick the best final answer.
- `abe debate --protocol synthesis ...` uses the chairman to merge answers.
- `abe validate ...` is a single-reviewer second opinion and may return prose without a structured pass/fail verdict.

Bob now calls Abe through `judge.mode: validate | debate`. In `debate` mode Bob forces Abe's decisive judge path:

```text
abe debate --json --protocol judge -- <statement>
```

Use this Bob config when the reviewer should make a decisive model-mediated call:

```yaml
judge:
  mode: debate
```
