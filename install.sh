#!/usr/bin/env bash
# Build bob (release) and install it to ~/.local/bin, then report prerequisites.
set -euo pipefail

cd "$(dirname "$0")"
BIN_DIR="${BOB_BIN_DIR:-$HOME/.local/bin}"

echo "Building bob (release)…"
cargo build --release

mkdir -p "$BIN_DIR"
install -m 0755 target/release/bob "$BIN_DIR/bob"
echo "Installed: $BIN_DIR/bob"

case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) echo "NOTE: $BIN_DIR is not on your PATH — add it (e.g. export PATH=\"$BIN_DIR:\$PATH\")." ;;
esac

echo
echo "Prerequisites (bob shells out to these):"
for c in git opencode abe; do
  if command -v "$c" >/dev/null 2>&1; then echo "  [ok]      $c"; else echo "  [MISSING] $c"; fi
done

echo
echo "Next:  cd your-project && bob init && \$EDITOR bob.yaml && bob doctor"
echo "Agents: Codex loads .codex-plugin/plugin.json + .mcp.json; opencode loads opencode.json."
