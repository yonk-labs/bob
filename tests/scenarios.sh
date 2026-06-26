#!/usr/bin/env bash
# Fast, no-LLM behavioral test of the real bob binary. Drives bob with stub
# builder/judge scripts to assert exit codes + outcomes across the scenario
# matrix. Run: `cargo build && tests/scenarios.sh`. (The live opencode+abe
# end-to-end test is the gated `tests/integration_build.rs`.)
set -u
HERE="$(cd "$(dirname "$0")/.." && pwd)"
BOB="$HERE/target/release/bob"; [ -x "$BOB" ] || BOB="$HERE/target/debug/bob"
[ -x "$BOB" ] || { echo "build bob first: cargo build"; exit 1; }

ROOT=$(mktemp -d); STUB="$ROOT/stubs"; mkdir -p "$STUB"
N=0; PASS=0; FAIL=0; LAST_DIR=""

cat > "$STUB/builder.sh" <<'EOS'
#!/usr/bin/env bash
WT="$3"   # invoked as: <cmd> run --dir <WT> <prompt>
case "${BUILDER_MODE:-edit}" in
  edit)   printf 'pub fn add(a: i32, b: i32) -> i32 { a + b }\n#[test]\nfn t() { assert_eq!(add(2,2),4); }\n' > "$WT/src/lib.rs" ;;
  noop)   : ;;
  secret) printf 'static K: &str = "AKIAIOSFODNN7EXAMPLE";\n' > "$WT/leak.txt" ;;
  many)   for i in $(seq 1 25); do echo "x$i" > "$WT/f$i.txt"; done ;;
  slow)   sleep 999 ;;
esac
exit 0
EOS
chmod +x "$STUB/builder.sh"

cat > "$STUB/judge.sh" <<'EOS'
#!/usr/bin/env bash
case "${JUDGE_MODE:-ok}" in
  fail) echo "judge boom" >&2; exit 1 ;;
  *)    printf '{"reviewer":"fake","take":"advisory: looks plausible"}\n' ;;
esac
EOS
chmod +x "$STUB/judge.sh"

mkfix() {
  local d="$ROOT/fix$N"; mkdir -p "$d/src"
  printf 'pub fn add(a: i32, b: i32) -> i32 { 0 }\n#[test]\nfn t() { assert_eq!(add(2,2),4); }\n' > "$d/src/lib.rs"
  printf '[package]\nname="it"\nversion="0.1.0"\nedition="2021"\n' > "$d/Cargo.toml"
  printf '/target\n/.bob\n' > "$d/.gitignore"
  {
    echo "builder: { cmd: \"$STUB/builder.sh\", timeout_secs: 1 }"
    echo "judge: { cmd: \"$STUB/judge.sh\", mode: validate, timeout_secs: 5 }"
    echo "$1"
    echo "loop: { max_iterations: 3, max_walltime_secs: 60 }"
    echo "scope: { max_changed_files: 20, max_changed_lines: 800, allow_paths: [] }"
    echo "apply: false"
    echo "artifacts: { dir: .bob/runs }"
  } > "$d/bob.yaml"
  ( cd "$d" && git init -q && git config user.email t@t && git config user.name t && git add . && git commit -qm init )
  echo "$d"
}

scen() { # name bm jm verify_yaml expected_exit grep_pat [extra args...]
  N=$((N+1)); local name="$1" bm="$2" jm="$3" vy="$4" eexit="$5" gpat="$6"; shift 6
  LAST_DIR=$(mkfix "$vy"); local out rc
  out=$(cd "$LAST_DIR" && BUILDER_MODE="$bm" JUDGE_MODE="$jm" "$BOB" build "Implement add" "$@" 2>&1); rc=$?
  local ok=1; [ "$rc" = "$eexit" ] || ok=0
  [ -n "$gpat" ] && { echo "$out" | grep -qE "$gpat" || ok=0; }
  if [ "$ok" = 1 ]; then PASS=$((PASS+1)); printf 'PASS  %-36s exit=%s\n' "$name" "$rc"
  else FAIL=$((FAIL+1)); printf 'FAIL  %-36s exit=%s(want %s) pat=[%s]\n      tail: %s\n' "$name" "$rc" "$eexit" "$gpat" "$(echo "$out"|tail -1)"; fi
}

V_TRUE='verify: { cmds: ["true"] }'; V_FALSE='verify: { cmds: ["false"] }'
V_EMPTY='verify: { cmds: [] }'; V_CARGO=$'verify:\n  cmds:\n    - cargo test'

echo "=== bob behavioral matrix (real binary, stub builder/judge) ==="
scen "converge+apply"               edit ok   "$V_TRUE"  0 "CONVERGED in" --apply
grep -q 'a + b' "$LAST_DIR/src/lib.rs" && echo "      -> applied to tree: YES" || { echo "      -> APPLY MISSING"; FAIL=$((FAIL+1)); }
scen "propose (no --apply)"         edit ok   "$V_TRUE"  0 "CONVERGED in"
grep -q '{ 0 }' "$LAST_DIR/src/lib.rs" && echo "      -> tree unchanged: YES" || { echo "      -> PROPOSE LEAKED"; FAIL=$((FAIL+1)); }
scen "no-converge (empty diff)"     noop ok   "$V_TRUE"  1 "NOT CONVERGED"
scen "verify gate fails"            edit ok   "$V_FALSE" 1 "NOT CONVERGED"
scen "scope exceeded (25 files)"    many ok   "$V_TRUE"  1 "NOT CONVERGED"
scen "builder timeout"              slow ok   "$V_TRUE"  1 ""
scen "judge fail = advisory only"   edit fail "$V_TRUE"  0 "CONVERGED in" --apply
scen "no verify gate converges"     edit ok   "$V_EMPTY" 0 "CONVERGED in" --apply
scen "real cargo-test gate"         edit ok   "$V_CARGO" 0 "CONVERGED in" --apply
scen "secret blocks apply"          secret ok "$V_TRUE"  0 "secret-scan flagged" --apply
[ -f "$LAST_DIR/leak.txt" ] && { echo "      -> SECRET LEAKED"; FAIL=$((FAIL+1)); } || echo "      -> secret not applied: YES"

echo; echo "=== $PASS/$N passed, $FAIL failed ==="
rm -rf "$ROOT"; [ "$FAIL" = 0 ]
