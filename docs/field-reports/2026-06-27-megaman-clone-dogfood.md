# Field Report: Mega Man Clone Dogfood Run
**Date:** 2026-06-27
**Tester:** hector + bob pipeline, qwen-193 and gemma-133 local models
**Goal:** End-to-end test of hector→bob loop on a real web platformer project

## What Worked
- hector wrote a focused test (`tests/body.test.js`) using repo conventions
- Red probe correctly identified the test as failing (no `src/body.js` existed)
- Campaign YAML was frozen with test as reference_path, src/body.js as editable
- bob's scope guard correctly blocked attempts to modify `tests/body.test.js`
- Both qwen and gemma eventually produced correct Body implementations (matching each other)
- hector's curl-based model client worked with zero new dependencies

## Issues Found (10)

### Critical (block normal use)
1. **node_modules not gitignored** — `npm install` created ~2000 files. Bob's scope check failed because path allowlist didn't exclude them. Bob wasted iterations on scope-exceeded errors.
   - **Fix:** bob doctor now detects + auto-fixes with `BOB_DOCTOR_FIX=1`. ✅ shipped.

2. **Bob applied broken implementation** — investigated. Bob's `next_action` logic at `engine.rs:70-138` already enforces verify-before-apply: `VerifyFailed` step returns `Continue` or `Stop`, never `Apply`. My earlier observation was wrong — the broken body.js on disk was from a previous run, not bob's apply. Bob refused to apply due to scope-exceeded (tried to rewrite tests).
   - **No fix needed.** Verify-before-apply is correctly enforced.

### Major (degrades experience)
3. **Model produced non-standard physics math** — model wrote test expecting `y=-0.49` after one tick with `gravity=-9.8, dt=0.1`. That's `0.5*g*dt²` formula (Verlet-style averaging), not standard Euler. Implementation used standard Euler → values disagreed.
   - **Fix:** hector's red-probe should validate that test math matches an explicit integration method. Add "integration method" field to test schema.

4. **Model produced mismatched API between test and implementation** — qwen wrote test using object args; implementation also used object args. Gemma wrote test using positional args; implementation also positional. But qwen's test (my baseline) and qwen's implementation used different APIs from gemma's pair. The model isn't self-consistent.
   - **Fix:** when model writes test, capture API signature. When model writes impl, validate it matches the captured signature.

5. **Bob tries to rewrite test files** — even when tests/ is NOT in editable_paths, bob's prompt includes the test file as context and the model decides to "improve" it. Scope guard blocks it, but the attempt wastes an iteration.
   - **Fix:** bob should explicitly tell the model "DO NOT modify test files" in the prompt.

### Minor (cosmetic)
6. **No retry_on_fail feedback loop working** — bob produced same wrong code twice across iterations. Abe-as-judge didn't catch the wrong implementation (probably not enabled in my command).
   - **Fix:** ensure retry_on_fail is the default, with clear feedback of WHY the verify failed.

7. **No artifact visibility** — bob applied (or tried to apply) and I had to read `.bob/runs/.../iter-0/diff.patch` to see what the model produced. No stdout summary.
   - **Fix:** bob should print "what would change" before applying.

8. **Hector red-probe accepted wrong test** — model wrote test with physics math that would never pass with any reasonable implementation. Red probe ran, test failed (compile error from missing module), hector marked it "red" and accepted. Should have caught the bad math.
   - **Fix:** hector red-probe should run a sanity check: if test doesn't have assertions about basic behaviors (constructor sets fields), reject.

9. **No manual apply from bob's diff** — had to copy src/body.js content from `.bob/runs/.../diff.patch` into the file manually.
   - **Fix:** add `bob apply-diff <run-id>` command that applies a specific run's diff.

10. **Single-class tasks exceed local model capacity** — both qwen and gemma struggled with a ~80-line physics class. Real game has 6+ classes (player, collision, projectile, enemy, damage, game loop).
    - **Fix:** keep slices small (under 50 lines per slice), prefer multiple small slices over one large.

## Recommendations
- Ship fixes #1 and #2 immediately (critical)
- Ship fix #5 in same release (trivial, high impact)
- Defer fixes #3, #4, #8 until after hector dispatch lands (need more usage data)
- Add token capture to bob (was in earlier plan) to compare model cost/quality on real game slices