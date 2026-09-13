#!/usr/bin/env bash
# S68 verification: the Rust tdjson/tdclient layer answers the request
# corpus byte-identically to the real C++ td_json_client — every response
# form the JSON ABI can produce: synchronous getOption, 400 error paths
# (parse errors, unknown types, non-synchronous methods), getJsonValue/
# getJsonString emission, the log stream/verbosity/tags functions,
# testReturnError, @extra/@client_id splicing through the legacy and the
# new client_id APIs.
#
# The C++ side links the pre-built libtdjson from the pinned checkout's
# cmake build (TD_BUILD); getOption("commit_hash") is excluded from the
# corpus because the C++ reports its build's git hash while the Rust port
# reports "unknown" until a build-script probe exists (S79).
#
# Usage: ./verify.sh [td-repo-root] (default: ../../../td).
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
TD_ROOT=${1:-"$SCRIPT_DIR/../../../td"}
TD_BUILD="${TD_BUILD_DIR:-$TD_ROOT/TD_BUILD}"
RS_ROOT="$SCRIPT_DIR/../.."
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

if [ ! -f "$TD_BUILD/libtdjson.so" ]; then
  echo "libtdjson.so missing in $TD_BUILD — build the C++ tdjson target first:" >&2
  echo "  cd $TD_ROOT/TD_BUILD && cmake --build . -j --target tdjson" >&2
  exit 1
fi

echo "== building the C++ dump against libtdjson =="
# tdjson_export.h is generated into the build dir by cmake.
c++ -O1 -o "$WORK/dump_tdjson" "$SCRIPT_DIR/cpp/dump_tdjson.cpp" \
  -I"$TD_BUILD" -I"$TD_ROOT" -L"$TD_BUILD" -ltdjson

echo "== building the Rust tdjson-oracle =="
env -u RUSTC_WRAPPER cargo build -q --release \
  --manifest-path "$RS_ROOT/conformance/Cargo.toml" --bin tdjson-oracle
RUST_BIN="${CARGO_TARGET_DIR:-$RS_ROOT/conformance/target}/release/tdjson-oracle"

LD_LIBRARY_PATH="$TD_BUILD" "$WORK/dump_tdjson" >"$WORK/c.raw" 2>/dev/null
"$RUST_BIN" >"$WORK/rs.raw"

# Documented interim normalizations, applied to BOTH sides:
# 1. The C++ client emits its startup state as updateOption and the initial
#    updateAuthorizationState updates before answering requests; the Rust Td
#    actor lands in S70. Drop those lines until then.
# 2. Client ids come from the C++ process-wide MultiImpl::create_id counter,
#    which the legacy client also consumes (the Rust counter is per-manager
#    until the S70 port). Normalize the number.
normalize() {
  grep -v '"@type":"updateOption"' |
    grep -v '"@type":"updateAuthorizationState"' |
    sed 's/"@client_id":[0-9]*/"@client_id":X/'
}
normalize <"$WORK/c.raw" >"$WORK/c.txt"
normalize <"$WORK/rs.raw" >"$WORK/rs.txt"

if ! cmp -s "$WORK/c.txt" "$WORK/rs.txt"; then
  echo "MISMATCH:" >&2
  diff "$WORK/c.txt" "$WORK/rs.txt" | head -60 >&2
  exit 1
fi
printf 'OK  %d lines byte-identical\n' "$(wc -l <"$WORK/c.txt")"
echo "S68 verification OK"
