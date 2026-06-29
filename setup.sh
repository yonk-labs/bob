#!/usr/bin/env bash
# Unified installer for the bob + hector + abe + opencode ecosystem.
# Checks prerequisites, builds all three tools, writes configs, registers MCP,
# and prints next steps. Idempotent — safe to re-run.
#
# Usage:
#   ./setup.sh                    # install everything
#   ./setup.sh --check            # check prerequisites only, don't install
#   ./setup.sh --repo-dir ~/yonk-tools  # custom repo parent dir
#
set -euo pipefail

CHECK_ONLY=0
REPO_DIR="${REPO_DIR:-$(cd "$(dirname "$0")/.." && pwd)}"
BIN_DIR="${HOME}/.local/bin"

for arg in "$@"; do
  case "$arg" in
    --check) CHECK_ONLY=1 ;;
    --repo-dir) shift; REPO_DIR="$1" ;;
  esac
done

BOB_DIR="$REPO_DIR/bob"
HECTOR_DIR="$REPO_DIR/hector"
ABE_DIR="${ABE_DIR:-$REPO_DIR/debator}"

# ── Colors ───────────────────────────────────────────────────────────────────
G() { printf "\033[32m%s\033[0m\n" "$*"; }
Y() { printf "\033[33m%s\033[0m\n" "$*"; }
R() { printf "\033[31m%s\033[0m\n" "$*"; }
B() { printf "\033[34m%s\033[0m\n" "$*"; }

# ── Step 1: Check prerequisites ─────────────────────────────────────────────
B "=== Checking prerequisites ==="

check() {
  if command -v "$1" &>/dev/null; then
    G "  [ok] $1 ($(command -v "$1"))"
    return 0
  else
    R "  [MISSING] $1 — $2"
    return 1
  fi
}

PREREQ_OK=1
check git "required for worktree isolation" || PREREQ_OK=0
check curl "required for model API calls" || PREREQ_OK=0
check cargo "required to build bob/hector/abe (install Rust: https://rustup.rs)" || PREREQ_OK=0

# Optional tools
echo ""
B "=== Optional tools ==="
check opencode "builder CLI (install: curl -fsSL https://opencode.ai/install | bash)" || true
check goose "alternative builder (install: https://github.com/block/goose)" || true
check abe "judge/debate CLI (will be installed below)" || true
check bob "build-verify-judge loop (will be installed below)" || true
check hector "TDD planner (will be installed below)" || true
check node "required for JS/Jest projects" || true
check npm "required for JS/Jest projects" || true

if [ "$PREREQ_OK" -eq 0 ]; then
  R ""
  R "Prerequisites missing. Install them and re-run."
  exit 1
fi

if [ "$CHECK_ONLY" -eq 1 ]; then
  G ""
  G "Prerequisites OK. Re-run without --check to install."
  exit 0
fi

# ── Step 2: Build and install tools ─────────────────────────────────────────
echo ""
B "=== Building and installing tools ==="

install_rust_bin() {
  local name="$1"
  local dir="$2"
  if [ ! -d "$dir" ]; then
    Y "  $name: source dir not found ($dir), skipping"
    return 0
  fi
  G "  Building $name (release)…"
  (cd "$dir" && cargo build --release 2>&1 | tail -1)
  install -m 0755 "$dir/target/release/$name" "$BIN_DIR/$name"
  G "  Installed: $BIN_DIR/$name"
}

mkdir -p "$BIN_DIR"
install_rust_bin bob "$BOB_DIR"
install_rust_bin hector "$HECTOR_DIR"
install_rust_bin abe "$ABE_DIR"

# ── Step 3: Write starter configs (don't overwrite existing) ────────────────
echo ""
B "=== Writing configs ==="

write_if_missing() {
  local path="$1"
  local content="$2"
  if [ -f "$path" ]; then
    Y "  $path: already exists, skipping"
  else
    mkdir -p "$(dirname "$path")"
    printf "%s" "$content" > "$path"
    G "  Created: $path"
  fi
}

# Bob config
BOB_CONFIG='# Bob config — four-tier builder system.
# Edit models/endpoints for your setup, then run: bob doctor
builder:
  cmd: opencode
  timeout_secs: 120
  models:
    # Add your model aliases here (alias: provider/model)
    # gemma: 192.168.1.133/your-model
    # qwen: ollama/your-model
  tiers:
    cheap: []       # thin builder — tiny edits, single-shot
    medium: []      # goose builder — reads files, iterates
    large: []       # goose builder — 80B+ models
    frontier: []    # opencode — cloud models, coding-plan subs
    default_tier: cheap
    cheap_builder: thin
    medium_builder: goose
    large_builder: goose
    frontier_builder: opencode
  escalation_policy: tier
  fallback_models: []
  args: []
judge:
  cmd: abe
  mode: validate
  timeout_secs: 60
  policy: advisory
verify:
  cmds: []   # e.g. ["cargo test", "npx jest"]
loop:
  max_iterations: 3
  max_walltime_secs: 600
scope:
  max_changed_files: 4
  max_changed_lines: 200
  allow_paths: []
apply: false
artifacts:
  dir: .bob/runs
'
write_if_missing "bob.yaml" "$BOB_CONFIG"

# Hector config
HECTOR_CONFIG='# Hector config — TDD planner with LLM-backed test writing.
# Add your model endpoints, then run: hector plan --task "..." --spec spec.md
models:
  # - { name: gemma, model: "your-model", base_url: "http://your-endpoint:8000/v1" }
  # - { name: minimax, model: "MiniMax-M3", base_url: "https://api.minimax.io/v1", api_key_env: MINIMAX_API_KEY }
default_model: null

verify:
  prefer_focused: true
scope:
  forbid_dependency_churn: true
  default_max_changed_files: 2
  default_max_changed_lines: 160
judge:
  default_policy: retry_on_fail
bob:
  campaign_auto_commit: true
review:
  deep_reviewer: null   # set to an abe reviewer name (e.g. "codex") for tier-2 review
  deep_on_accept: false
'
write_if_missing "hector.yaml" "$HECTOR_CONFIG"

# Abe config
ABE_CONFIG='# Abe config — model debate/validation.
# Add your models, then run: abe debate "your question"
defaults:
  timeout_secs: 60
  max_tokens: 1024
models:
  # - { name: gemma, kind: openai-compatible, model: "your-model", base_url: "http://your-endpoint:8000/v1" }
  # - { name: codex, kind: cli, cli: codex }
validate:
  reviewers: []
'
write_if_missing "abe.yaml" "$ABE_CONFIG"

# ── Step 4: Register MCP servers in opencode (if opencode installed) ────────
echo ""
B "=== Registering MCP servers ==="

if command -v opencode &>/dev/null; then
  OC_CONFIG="${HOME}/.config/opencode/config.json"
  if [ -f "$OC_CONFIG" ]; then
    if python3 -c "import json; d=json.load(open('$OC_CONFIG')); print('bob' in d.get('mcp',{}))" 2>/dev/null | grep -q True; then
      Y "  opencode MCP: bob already registered"
    else
      G "  opencode MCP: registering bob + abe"
      python3 -c "
import json
path = '$OC_CONFIG'
cfg = json.load(open(path))
cfg.setdefault('mcp', {})
cfg['mcp']['bob'] = {'type': 'local', 'command': ['bob', 'mcp']}
cfg['mcp']['abe'] = {'type': 'local', 'command': ['abe', 'mcp']}
json.dump(cfg, open(path, 'w'), indent=2)
print('  done')
" 2>/dev/null || Y "  opencode MCP: could not auto-register (add manually)"
    fi
  else
    Y "  opencode config not found at $OC_CONFIG — create it first"
  fi
else
  Y "  opencode not installed — skipping MCP registration"
fi

# ── Step 5: Verify PATH ─────────────────────────────────────────────────────
echo ""
B "=== PATH check ==="
case ":$PATH:" in
  *":$BIN_DIR:"*) G "  $BIN_DIR is on PATH" ;;
  *) Y "  $BIN_DIR is NOT on PATH. Add: export PATH=\"$BIN_DIR:\$PATH\"" ;;
esac

# ── Step 6: Run doctor checks ───────────────────────────────────────────────
echo ""
B "=== Doctor checks ==="

for tool in bob hector abe; do
  if command -v "$tool" &>/dev/null; then
    if [ "$tool" = "bob" ]; then
      "$tool" doctor 2>&1 | head -10
    else
      G "  [ok] $tool installed"
    fi
  else
    R "  [MISSING] $tool"
  fi
done

# ── Step 7: Print next steps ────────────────────────────────────────────────
echo ""
B "=== Next steps ==="
cat << 'NEXT'

1. Edit configs for your model endpoints:
   - bob.yaml    → add models + tiers
   - hector.yaml → add model endpoints
   - abe.yaml    → add reviewer models

2. Create a .gitignore for your project:
   .bob/
   node_modules/

3. Test the pipeline:
   # Write a spec, then:
   hector plan --task "implement X" --spec spec.md
   bob build "implement X" --files tests/x.test.js --verify "npx jest"
   hector review --campaign campaign.yaml --bob-result result.json

4. Run a parallel campaign:
   hector dispatch --file campaign.yaml --jobs 4

5. Check model performance:
   bob stats

6. Clean up orphaned processes:
   bob reap

Docs: bob/docs/, hector/HECTOR_SPEC.md, debator/README.md
NEXT

G ""
G "Setup complete. Edit the configs and start building."
