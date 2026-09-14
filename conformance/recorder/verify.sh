#!/usr/bin/env bash
# S73/S74 verification: the recorder wraps the real C++ td_json_client
# (libtdjson), replays the corpus requests and records the output stream;
# the replayer does the same through the Rust tdclient layer; the streams
# must match modulo the documented nondeterminism.
#
# Corpus v0 covers the offline surface (synchronous requests, the preauth
# option handlers, the WaitParameters gating, the startup state). The live
# test-DC corpus (TD_RECORD_LIVE=1 + TD_RECORD_* credentials) records
# real sessions once credentials are provided.
#
# Usage: ./verify.sh [td-repo-root] (default: ../../../td).
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
TD_ROOT=${1:-"$SCRIPT_DIR/../../../td"}
TD_BUILD="${TD_BUILD_DIR:-$TD_ROOT/TD_BUILD}"
RS_ROOT="$SCRIPT_DIR/../.."
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

CORPUS="${CORPUS_FILE:-$SCRIPT_DIR/corpus/v0_requests.txt}"

echo "== building the C++ recorder against libtdjson =="
c++ -O1 -o "$WORK/record" "$SCRIPT_DIR/cpp/record.cpp" \
  -I"$TD_BUILD" -I"$TD_ROOT" -L"$TD_BUILD" -ltdjson

echo "== building the Rust replayer =="
# Debug profile: the release codegen of the generated td_api model is
# prohibitively slow locally; CI builds release once for the cached gate.
env -u RUSTC_WRAPPER cargo build -q \
  --manifest-path "$RS_ROOT/conformance/Cargo.toml" --bin recorder-replayer
RUST_BIN="${CARGO_TARGET_DIR:-$RS_ROOT/conformance/target}/debug/recorder-replayer"

LD_LIBRARY_PATH="$TD_BUILD" "$WORK/record" <"$CORPUS" >"$WORK/c.txt" 2>/dev/null
"$RUST_BIN" <"$CORPUS" >"$WORK/rs.txt"

# Documented interim normalization, applied to BOTH sides: the C++ reports
# its build's git hash for the commit_hash option while the Rust port
# reports "unknown" until a build-script probe exists (S79).
normalize() {
  sed 's/"commit_hash","value":{"@type":"optionValueString","value":"[^"]*"/"commit_hash","value":{"@type":"optionValueString","value":"<git>"/'
}
normalize <"$WORK/c.txt" >"$WORK/c.norm"
normalize <"$WORK/rs.txt" >"$WORK/rs.norm"

if ! cmp -s "$WORK/c.norm" "$WORK/rs.norm"; then
  echo "MISMATCH:" >&2
  diff "$WORK/c.norm" "$WORK/rs.norm" | head -60 >&2
  exit 1
fi
printf 'OK  %d records byte-identical\n' "$(wc -l <"$WORK/c.norm")"
echo "S73/S74 verification OK"
