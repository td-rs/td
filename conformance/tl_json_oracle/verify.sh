#!/usr/bin/env bash
# S12 verification: the tl-codegen-generated td_api JSON serializer/parser
# must match td's own generated td_api_json_*.cpp output byte for byte on a
# sample set covering every emission form — @type header, int32/int53 plain
# numbers, int64 as quoted strings (td's JsonInt64), doubles with fixed 6
# decimals (incl. -0.0 and round-to-zero), bools, the full string escape
# range, bytes as standard base64, custom structs/enums, vector<int32>,
# vector<string>, vector<int64> (quoted), vector<enum>, vector<custom> —
# and must parse the same lenient request JSONs (missing/null fields,
# ints-as-strings, numeric @type ids, unknown fields) into byte-identical
# binary stores, with the same error messages on malformed input.
#
# The C++ oracle compiles td's checked-in generated td_api_json_*.cpp and
# td_api_*.cpp plus the minimal tdutils runtime against the unmodified td
# sources. Usage: ./verify.sh [td-repo-root] (default: ../../../td).
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
TD_ROOT=${1:-"$SCRIPT_DIR/../../../td"}
GENERATED_DIR="$TD_ROOT/td/generate/auto/td/telegram"
TDUTILS_DIR="$TD_ROOT/tdutils"
RS_ROOT="$SCRIPT_DIR/../.."
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

[ -f "$GENERATED_DIR/td_api_json_0.cpp" ] || {
  echo "not found: $GENERATED_DIR/td_api_json_0.cpp (pass the td repo root)" >&2
  exit 1
}

echo "== building the C++ oracle from td's generated td_api_json =="
# -w: generated code trips warnings; only behavior matters here.
# $SCRIPT_DIR first on the include path: it shadows td/utils/config.h with
# the minimal all-features-off stand-in (same trick as S04/S10).
c++ -O1 -w -o "$WORK/dump_json" "$SCRIPT_DIR/dump_json.cpp" \
  "$GENERATED_DIR"/td_api_json_*.cpp \
  "$GENERATED_DIR"/td_api_[0-9].cpp \
  "$TDUTILS_DIR"/td/utils/logging.cpp \
  "$TDUTILS_DIR"/td/utils/Time.cpp \
  "$TDUTILS_DIR"/td/utils/Status.cpp \
  "$TDUTILS_DIR"/td/utils/TsCerr.cpp \
  "$TDUTILS_DIR"/td/utils/TsLog.cpp \
  "$TDUTILS_DIR"/td/utils/ExitGuard.cpp \
  "$TDUTILS_DIR"/td/utils/FlatHashTable.cpp \
  "$TDUTILS_DIR"/td/utils/filesystem.cpp \
  "$TDUTILS_DIR"/td/utils/PathView.cpp \
  "$TDUTILS_DIR"/td/utils/Random.cpp \
  "$TDUTILS_DIR"/td/utils/StackAllocator.cpp \
  "$TDUTILS_DIR"/td/utils/tl_parsers.cpp \
  "$TDUTILS_DIR"/td/utils/check.cpp \
  "$TDUTILS_DIR"/td/utils/StringBuilder.cpp \
  "$TDUTILS_DIR"/td/utils/Slice.cpp \
  "$TDUTILS_DIR"/td/utils/buffer.cpp \
  "$TDUTILS_DIR"/td/utils/misc.cpp \
  "$TDUTILS_DIR"/td/utils/utf8.cpp \
  "$TDUTILS_DIR"/td/utils/unicode.cpp \
  "$TDUTILS_DIR"/td/utils/JsonBuilder.cpp \
  "$TDUTILS_DIR"/td/utils/base64.cpp \
  "$TDUTILS_DIR"/td/utils/port/*.cpp \
  "$TDUTILS_DIR"/td/utils/port/detail/NativeFd.cpp \
  "$TDUTILS_DIR"/td/utils/port/detail/ThreadIdGuard.cpp \
  -I"$SCRIPT_DIR" -I"$GENERATED_DIR/../.." -I"$TD_ROOT" -I"$TDUTILS_DIR"

echo "== building the Rust dump-td-api-json =="
env -u RUSTC_WRAPPER cargo build -q --release --manifest-path "$RS_ROOT/Cargo.toml" \
  -p tdclient --bin dump_td_api_json
RUST_BIN="${CARGO_TARGET_DIR:-$RS_ROOT/target}/release/dump_td_api_json"

"$WORK/dump_json" >"$WORK/c.txt"
"$RUST_BIN" >"$WORK/rs.txt"
if ! cmp -s "$WORK/c.txt" "$WORK/rs.txt"; then
  echo "MISMATCH:" >&2
  diff "$WORK/c.txt" "$WORK/rs.txt" | head -40 >&2
  exit 1
fi
printf 'OK  %d lines byte-identical\n' "$(wc -l <"$WORK/c.txt")"
echo "S12 verification OK"
