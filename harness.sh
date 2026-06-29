#!/usr/bin/env bash
# harness.sh — frontier plan → abe debate → hector check → bob campaign → hector review
#
# The frontier model writes campaign.yaml (use `hector frontier-brief` as the
# format guide), then this script runs the deterministic后半段:
#   1. abe debates the whole campaign (optional, --debate): 3 models flag spec gaps
#   2. hector check: static reject of weak slices (blocks)
#   3. bob campaign: serial verified build with abe-as-judge inside (retry_on_fail)
#   4. hector review: accept / split_task / revise_campaign / ask_human
#
# Artifacts land next to the campaign: <name>.debate.md, .bob-result.json, .review.json
# Exit: 0 if hector says accept/accept_for_human_review, 1 otherwise.
set -euo pipefail

campaign="${1:?usage: harness.sh <campaign.yaml> [--debate] [--apply]}"
shift
debate=0; apply=0
for arg in "$@"; do
  case "$arg" in
    --debate) debate=1 ;;
    --apply)  apply=1  ;;
    *) echo "unknown flag: $arg" >&2; exit 2 ;;
  esac
done

[[ -f "$campaign" ]] || { echo "campaign not found: $campaign" >&2; exit 2; }
stem="${campaign%.yaml}"

# Stage 1 — abe debates the campaign (gate, not a rewriter; frontier reads .debate.md)
if [[ "$debate" -eq 1 ]]; then
  echo "=== abe debating campaign (3 models) ===" >&2
  abe debate \
    "Review this Bob campaign YAML for implementation risks. For EACH slice list:
     (a) ambiguous acceptance criteria, (b) missing edge cases or invalid states,
     (c) verify_cmds that won't actually prove the contract, (d) editable_paths too broad or too narrow.
     Be specific. End with VERDICT: proceed | revise | split." \
    --files "$campaign" \
    --rounds 1 --protocol synthesis > "${stem}.debate.md" 2>/dev/null || true
  echo "  debate report: ${stem}.debate.md" >&2
  grep -qiE "VERDICT:.*revise|VERDICT:.*split" "${stem}.debate.md" 2>/dev/null && {
    echo "abe flagged revise/split — review ${stem}.debate.md before proceeding." >&2
    exit 1
  }
fi

# Stage 2 — hector static check (hard gate)
echo "=== hector check ===" >&2
hector check --file "$campaign"

# Stage 3 — bob campaign (abe judges inside via retry_on_fail; auto_commit per slice)
echo "=== bob campaign ===" >&2
apply_flag=""
[[ "$apply" -eq 1 ]] && apply_flag="--apply"
bob campaign --file "$campaign" $apply_flag > "${stem}.bob-result.json" || true

# Stage 4 — hector review (deterministic decision)
echo "=== hector review ===" >&2
hector review --campaign "$campaign" --bob-result "${stem}.bob-result.json" > "${stem}.review.json" 2>/dev/null || true
cat "${stem}.review.json"

echo "" >&2
echo "=== artifacts ===" >&2
[[ -f "${stem}.debate.md" ]] && echo "debate:   ${stem}.debate.md" >&2
echo "result:   ${stem}.bob-result.json" >&2
echo "review:   ${stem}.review.json" >&2

decision=$(python3 -c "import json;print(json.load(open('${stem}.review.json')).get('decision',''))" 2>/dev/null || echo "")
case "$decision" in
  accept|accept_for_human_review) exit 0 ;;
  *) exit 1 ;;
esac
