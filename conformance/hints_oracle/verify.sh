#!/usr/bin/env bash
# M.4.4 verification: the Rust Hints / translit / search-words port must answer
# the scenario byte-identically to td's UNMODIFIED Hints.cpp (tdlib ships no
# TEST(Hints, ...) — this differential run is the C++ oracle).
#
#  1. stages a config.h with every TD_HAVE_* off and compiles oracle.cpp
#     against the td checkout's tdutils sources;
#  2. builds the Rust twin (conformance crate, hints_driver bin);
#  3. runs both over scenario.txt and byte-compares the answers.
#
# Usage: ./verify.sh [td-repo-root]   (default: ../../../td relative to here)
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
TD_ROOT=${1:-"$SCRIPT_DIR/../../../td"}
SCENARIO="$SCRIPT_DIR/scenario.txt"
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

[ -f "$TD_ROOT/tdutils/td/utils/Hints.cpp" ] || { echo "not found: $TD_ROOT (pass the td repo root)" >&2; exit 1; }

mkdir -p "$WORK/td/utils"
sed 's/#cmakedefine01 \([A-Za-z0-9_]*\).*/#define \1 0/' \
  "$TD_ROOT/tdutils/td/utils/config.h.in" > "$WORK/td/utils/config.h"

echo "== building the C++ Hints oracle =="
TDUTILS="$TD_ROOT/tdutils/td/utils"
g++ -std=c++17 -O2 -I "$WORK" -I "$TD_ROOT/tdutils" \
  "$SCRIPT_DIR/oracle.cpp" \
  "$TDUTILS/Hints.cpp" "$TDUTILS/translit.cpp" "$TDUTILS/unicode.cpp" \
  "$TDUTILS/utf8.cpp" "$TDUTILS/logging.cpp" "$TDUTILS/StringBuilder.cpp" \
  "$TDUTILS/Status.cpp" "$TDUTILS/misc.cpp" "$TDUTILS/Slice.cpp" \
  -o "$WORK/oracle"

echo "== building the Rust driver =="
env -u RUSTC_WRAPPER cargo build -q --manifest-path "$SCRIPT_DIR/../Cargo.toml" \
  --bin hints_driver
DRIVER="$SCRIPT_DIR/../../target/debug/hints_driver"

echo "== running both over the scenario =="
"$WORK/oracle" < "$SCENARIO" > "$WORK/cpp.out"
"$DRIVER" < "$SCENARIO" > "$WORK/rust.out"

cmp "$WORK/cpp.out" "$WORK/rust.out"
echo "OK Hints oracle: $(wc -l < "$WORK/cpp.out") answers byte-identical"
