#!/usr/bin/env bash
# bench.sh — A/B/C test models on a coding task. Real token data from opencode.
#
# Proves whether different models differ enough (tokens, convergence, quality)
# to justify a parallel swarm. Same task, fresh worktree per model, real metrics.
#
# Usage:
#   bench.sh --repo /path/to/repo --task "implement X" --verify "cargo test x" \
#            --models qwen-193,gemma-133,glm
#
# Each model gets a fresh git worktree from the repo HEAD. opencode runs with
# --format json so we capture real token usage from step_finish events.
# Output: a comparison table + raw JSON results in bench-results/.
set -euo pipefail

repo=""; task=""; verify=""; models=""; timeout_secs=300
while [[ $# -gt 0 ]]; do
  case "$1" in
    --repo) repo="$2"; shift 2 ;;
    --task) task="$2"; shift 2 ;;
    --verify) verify="$2"; shift 2 ;;
    --models) models="$2"; shift 2 ;;
    --timeout) timeout_secs="$2"; shift 2 ;;
    *) echo "unknown: $1" >&2; exit 2 ;;
  esac
done

[[ -n "$repo" && -n "$task" && -n "$verify" && -n "$models" ]] || {
  echo "usage: bench.sh --repo PATH --task '...' --verify 'cmd' --models a,b,c" >&2
  exit 2
}

repo="$(cd "$repo" && pwd)"
outdir="$(pwd)/bench-results/$(date +%Y%m%d-%H%M%S)"
mkdir -p "$outdir"
base_sha="$(git -C "$repo" rev-parse HEAD)"
IFS=',' read -ra model_list <<< "$models"

echo "repo:     $repo"
echo "base:     ${base_sha:0:12}"
echo "task:     $task"
echo "verify:   $verify"
echo "models:   ${model_list[*]}"
echo "output:   $outdir"
echo ""

run_one() {
  local model="$1"
  local slug="${model//\//_}"
  local wt="$outdir/wt-$slug"
  git -C "$repo" worktree add -d "$wt" "$base_sha" >/dev/null 2>&1 || {
    echo "  $model: worktree creation failed" >&2
    return
  }

  local events="$outdir/$slug.events.jsonl"
  local start=$(date +%s)
  timeout "$timeout_secs" opencode run \
    --dir "$wt" \
    --model "$model" \
    --format json \
    "$task" > "$events" 2>"$outdir/$slug.stderr" || true
  local exit_code=$?
  local end=$(date +%s)
  local wall=$((end - start))

  # verify gate
  local verify_ok="fail"
  if (cd "$wt" && eval "$verify" >"$outdir/$slug.verify.log" 2>&1); then
    verify_ok="pass"
  fi

  # diff stats
  local diff_stat
  diff_stat=$(git -C "$wt" diff --numstat | awk '{f++; a+=$1; d+=$2} END {printf "%d %d %d", f+0, a+0, d+0}')

  # cleanup worktree
  git -C "$repo" worktree remove "$wt" --force 2>/dev/null || rm -rf "$wt"

  # emit one JSON line for this model
  python3 - "$model" "$events" "$wall" "$exit_code" "$verify_ok" "$diff_stat" <<'PY'
import json, sys, pathlib
model, events, wall, exit_code, verify_ok, diff_stat = sys.argv[1:7]
files, ins, dels = diff_stat.split()
tok_in = tok_out = tok_reas = 0
for line in pathlib.Path(events).read_text().splitlines():
    try:
        ev = json.loads(line)
    except Exception:
        continue
    if ev.get("type") == "step_finish":
        t = ev.get("part", {}).get("tokens", {})
        tok_in += t.get("input", 0)
        tok_out += t.get("output", 0)
        tok_reas += t.get("reasoning", 0)
print(json.dumps({
    "model": model, "wall_secs": int(wall), "exit_code": int(exit_code),
    "verify": verify_ok, "files_changed": int(files), "lines_added": int(ins),
    "lines_deleted": int(dels),
    "tokens_in": tok_in, "tokens_out": tok_out, "tokens_reasoning": tok_reas,
    "tokens_total": tok_in + tok_out + tok_reas,
}))
PY
}

echo "=== running ${#model_list[@]} models ==="
for model in "${model_list[@]}"; do
  echo "  $model..." >&2
  run_one "$model" | tee -a "$outdir/results.jsonl"
done

echo ""
echo "=== scoreboard ==="
column -t -s$'\t' < <(python3 - "$outdir/results.jsonl" <<'PY'
import json, sys
rows = [json.loads(l) for l in open(sys.argv[1]) if l.strip()]
rows.sort(key=lambda r: (r["verify"] != "pass", r["tokens_total"]))
hdr = ["model", "verify", "wall_s", "tok_in", "tok_out", "tok_reas", "tok_total", "files", "+lines", "-lines"]
print("\t".join(hdr))
for r in rows:
    print("\t".join(str(x) for x in [
        r["model"], r["verify"], r["wall_secs"],
        r["tokens_in"], r["tokens_out"], r["tokens_reasoning"], r["tokens_total"],
        r["files_changed"], r["lines_added"], r["lines_deleted"],
    ]))
PY
)

echo ""
echo "raw: $outdir/results.jsonl"
