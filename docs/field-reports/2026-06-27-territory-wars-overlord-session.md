# Field Report — Farming work to bob as "overlord"

**Date:** 2026-06-27
**Operator:** Claude Code (Opus 4.8), acting as orchestrator
**Target repo:** `~/yonk-apps/territory-wars` (Node/Express/better-sqlite3/Jest wrestling sim)
**bob version:** local build at `~/yonk-tools/bob`, builder = `opencode` → `ollama/Intel/Qwen3-Coder-Next-int4-AutoRound` (192.168.1.193 vLLM), judge = `abe validate`

This documents a real session where I drove bob as a subagent to implement a slice of feature
work (Plan 2: a JSON API over a roster-as-divisions engine model), while I stayed at the level of
design, spec-writing, verification, and integration.

---

## 1. Session state & next steps (territory-wars)

**Done and merged to `master`:**
- **Plan 1 (engine):** `computeDivisionPlan`, `rosterRequirementSummary`, `rosterShortfall`, contender-depth config. Pure domain, TDD.
- **Plan 2 (JSON API):** 4 endpoints on `src/routes/api/roster-plan.js`
  - `GET /api/roster-plan` — summary + shortfall
  - `POST /api/roster-plan/divisions/:id/contenders` — slot a contender (+ cross-promotion ownership guard)
  - `GET /api/roster-plan/divisions/:id` — division detail + ranked board
  - `GET /api/roster-plan/divisions/:id/candidates` — slottable wrestlers (`champ.eligibleContenders`)
- Infra: jest pinned to `--maxWorkers=50%`; `.bob/` excluded from jest module map.

**Next steps (in order):**
1. **Canonical full-suite green is pending a quiet box.** The shared 24-core machine sat at load 20–34 from external work; every full `npm test` flaked on timeouts (60–174s/suite), but every failing suite passes in isolation and none were mine. One clean 431-suite green was captured at load 8.6. Re-run `npm test` when `uptime` load < ~12 to record the official green.
2. **Plan 3 — SPA Roster/Divisions screen** (Vite + Svelte over the new API). Overlord-led: design + browser verification; bob can only take gate-testable pieces (API client unit tests, a parity helper).
3. **Open design decision (not a bob task):** an unslot/DELETE contender endpoint collides with the locked "Slot = commit; removing destroys" rule. Needs a human design call before any implementation.
4. Optionally keep farming **single-file** API slices to bob (dashboard slice, etc.).

**Environment notes for the next operator:**
- bob default builder = the 192.168.1.193 qwen (vLLM on :8000 via opencode's `ollama` provider). `ollama` is NOT installed locally; a native ollama on :11434 is a fallback endpoint.
- Never run two `npm test` concurrently (CPU oversubscription → false failures). A run takes ~170–320s at low load.
- If a bob run does not converge, it leaves `.bob/worktrees/*` + `bob/*` branches; clean them or jest throws a Haste collision on `@mod-games/sim-kernel`.

---

## 2. What I learned farming work to bob

**The winning pattern: test-as-spec + frozen scope.**
For each slice I (a) wrote a *failing* test that encodes the contract, (b) committed it, (c) set `scope.allow_paths` to the implementation dir so bob **could not edit the test to cheat the gate**, then (d) handed bob the task. bob converged the implementation until `npx jest <that file>` passed. The objective gate — not my judgment, not bob's — decided "done." This is the single most important lever: a precise, frozen, executable spec.

**Delegation scoreboard (5 `build` calls):**

| # | Task | Files | Model | Result |
|---|------|-------|-------|--------|
| 1 | `rosterShortfall` domain fn | 1 | qwen | converged, 2 iters |
| 2 | `GET /api/roster-plan` | 2 (route+mount) | qwen | converged, 1 iter |
| 3 | `POST .../contenders` | 1 | qwen | converged, 2 iters |
| 4 | `GET .../divisions/:id` | 1 | qwen | converged, 1 iter |
| 5 | `GET .../candidates` (+ domain fn) | 3 | qwen → minimax | **stalled** then **infra error** → I did it by hand |

**bob's envelope is now empirically clear:** single-file (or one-file-plus-trivial-mount) changes with one objective gate land reliably on a free local model, often in a single iteration. The one genuinely multi-file change (domain fn + facade export + route) stalled the local qwen (`EmptyDiffAfterCritique`) and the cloud fallback errored at the infra layer. Multi-file wiring is the operator's job.

**bob is "trust through verification," not "trust the model."** The local qwen is not a strong model, but it didn't need to be — the gate caught everything. A weak+free builder behind a hard gate beats a strong+expensive builder with no gate, for well-specified mechanical work.

**The builder is context-blind.** bob's builder sees only `task`/`spec`/`files`, never the conversation. That forced me to restate file conventions (helpers, imports, route placement) every time. Good discipline — it makes the spec self-contained — but verbose.

---

## 3. Critique

bob is excellent as a *verified single-file implementer*. The weakness is everything **around** the build loop — the orchestration surface is manual and has sharp edges.

- **Per-slice config churn.** The verify gate and `allow_paths` live in `bob.yaml` on disk. Every new slice meant hand-editing `bob.yaml` (and either committing the churn or reverting it). There's no per-run override for the two things that change most often.
- **Messy failure exits.** A non-converged or errored run leaves orphaned git worktrees and branches behind. In a JS/Jest repo those orphans cause a hard, cryptic Haste module-map collision — a failure mode far away from its cause. The minimax fallback failure surfaced only as `"builder exited with status exit status: 1"` with no diagnostic.
- **No graceful model degradation.** When the local builder stalled, escalation to the roster's `minimax` was a manual second call, and when that errored there was no chain to the next option — it just stopped.
- **`apply: false` is correct but adds toil.** Propose-mode is the right default (I reviewed and fixed a stray indentation before landing). But for a clean diff that already passed the gate and lint, manually re-applying with an editor is redundant work.
- **No native multi-step/queue concept.** bob does one task per invocation. The "loop over a backlog of slices" — the actual overlord pattern — lives entirely in the operator's head and hands.

---

## 4. Three suggestions to make bob better

1. **Per-run `verify` and `scope` overrides on the `build` call.**
   Add optional `verify_cmds` and `allow_paths` params to the `build` tool / CLI, overriding `bob.yaml` for that invocation only. This kills the #1 friction (editing `bob.yaml` every slice) and keeps the committed config stable. Example: `build({ task, verify_cmds: ["npx jest path/to/x.test.js"], allow_paths: ["src/routes/"] })`.

2. **Self-healing artifact lifecycle + test-runner hygiene.**
   On non-converge/error, auto-remove the run's worktree and branch (or ship `bob gc` and run it on the next invocation). And document/emit a one-line "add `.bob/` to your test runner's ignore list" hint on first run in a JS/TS repo — the Haste collision is a guaranteed stub-your-toe for any Jest user.

3. **A model-fallback chain with diagnostics.**
   Let `builder.models` act as an ordered fallback: on stall (`EmptyDiffAfterCritique`) or infra error, auto-advance to the next model with a logged reason ("qwen stalled after 2 iters → escalating to minimax"). Surface the underlying builder error (stderr tail), not just `exit status: 1`. Bonus: a `--retry-on-stall` that bumps `max_iters` once before escalating.

---

## 5. How to make bob more autonomous

The goal: encode the overlord loop so bob drains a backlog of slices across turns, pausing for a human only on genuine stalls/ambiguity.

**a) A "campaign" runner.** Feed bob a queue of `{ test_file, task, allow_paths }` tuples. For each: confirm the gate is RED → build → on converge, apply → run the focused gate GREEN → commit on a feature branch. Loop until the queue drains or a task stalls (then pause for human). This is exactly the loop I ran by hand.

**b) Trusted auto-apply + auto-commit, gated on objective signals.** Because the gate is objective, landing can be automated when ALL hold: (1) focused gate green, (2) diff within `scope.max_changed_*` caps, (3) lint clean, (4) secret-scan clean. Add `apply: true` + `auto_commit: true` modes that fire only under those conditions. Never auto-merge to `main` — open a PR.

**c) A planner front-end.** The piece I did manually was decomposing a plan doc into single-file TDD slices, each with a pre-written failing test. A planning agent that emits that queue (test file + task + scope per slice) from a plan/spec doc is what turns "build this feature" into "drain this backlog." Pair it with bob's executor and the human only reviews the plan and the final PR.

**d) Run it unattended.** Wrap the campaign in a `/loop` or workflow so it proceeds across turns; use push notifications to surface only stalls, scope-cap breaches, or ambiguous specs. Keep hard guardrails on: apply gated on green+scope+lint+secret-scan, PR-only (never main), a total-slice cap, and a wall-clock budget.

**The throughput ceiling is spec quality, not model quality.** Every converged slice converged because the failing test pinned the contract exactly. Autonomy scales precisely as far as the planner can produce frozen, executable specs — and no further. Invest there.
