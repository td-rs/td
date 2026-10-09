#!/usr/bin/env bash
# S84 scheme parity audit (release-process tooling): verifies the schemes
# the Rust codegen builds from still match the S03 import pin, and — when
# the network allows — reports drift against upstream tdlib/td.
#
# Usage: ./check.sh [td-repo-root]   (default: fetch-free local audit only;
# pass a tdlib checkout to audit against it, e.g. a fresh clone of tdlib/td)
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
RS_ROOT="$SCRIPT_DIR/../.."
SCHEMES="td_api.tl telegram_api.tl secret_api.tl e2e_api.tl mtproto_api.tl"
PINNED_COMMIT=c15d3f5a5de6e3ba5839822c451152e5e18bb700

echo "== local drift check: schemes vs the S03 sha256 pin =="
cd "$RS_ROOT/crates/tl-codegen/scheme"
if sha256sum -c sha256sums.txt; then
  echo "local: schemes match the S03 pin ($PINNED_COMMIT)"
else
  echo "FAIL: local schemes drifted from sha256sums.txt — the pin must be" >&2
  echo "updated deliberately, with the generated code re-audited (S84)." >&2
  exit 1
fi

TD_ROOT=${1:-}
if [ -z "$TD_ROOT" ]; then
  echo "upstream: skipped (no td checkout passed; pass one to audit drift)"
  exit 0
fi

echo "== upstream drift check vs $TD_ROOT =="
for scheme in $SCHEMES; do
  if [ ! -f "$TD_ROOT/$scheme" ] && [ ! -f "$TD_ROOT/td/generate/scheme/$scheme" ]; then
    echo "upstream: $scheme not found in $TD_ROOT — skipped"
    continue
  fi
  upstream=$(find "$TD_ROOT" -name "$scheme" | head -1)
  if cmp -s "$scheme" "$upstream"; then
    echo "upstream: $scheme identical"
  else
    echo "upstream: $scheme DIFFERS — scheme import + parity audit required (S84)"
  fi
done
