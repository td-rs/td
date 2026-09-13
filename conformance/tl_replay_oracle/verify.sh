#!/usr/bin/env bash
# S15 verification: the differential replay gate. The Rust corpus builders
# dump one binary sample per constructor (two depths each) for all four
# wire schemes; td's own generated C++ code fetches every type blob with
# its parser, requires an exact, error-free consumption, and re-stores
# every class that carries a binary store method — the bytes must come
# back identical. Functions are skipped (td generates no fetch for
# functions); their ids are pinned by the S07 dump and their layouts by the
# Rust-side round-trip tests.
#
# Usage: ./verify.sh [td-repo-root]   (default: ../../../td relative to here)
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
TD_ROOT=${1:-"$SCRIPT_DIR/../../../td"}
TELEGRAM_DIR="$TD_ROOT/td/generate/auto/td/telegram"
MTPROTO_DIR="$TD_ROOT/td/generate/auto/td/mtproto"
TDUTILS_DIR="$TD_ROOT/tdutils"
RS_ROOT="$SCRIPT_DIR/../.."
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

[ -f "$TELEGRAM_DIR/telegram_api_0.cpp" ] || {
  echo "not found: $TELEGRAM_DIR/telegram_api_0.cpp (pass the td repo root)" >&2
  exit 1
}

echo "== building the C++ replay oracle from td's generated schemes =="
# -w: generated code trips warnings; only behavior matters here.
# $SCRIPT_DIR first on the include path: it shadows td/utils/config.h with
# the minimal all-features-off stand-in (same trick as S04/S10/S12).
c++ -O1 -w -o "$WORK/replay" "$SCRIPT_DIR/replay.cpp" \
  "$TELEGRAM_DIR"/telegram_api_[0-9].cpp \
  "$TELEGRAM_DIR"/secret_api.cpp \
  "$TELEGRAM_DIR"/e2e_api.cpp \
  "$MTPROTO_DIR"/mtproto_api.cpp \
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
  "$TDUTILS_DIR"/td/utils/port/*.cpp \
  "$TDUTILS_DIR"/td/utils/port/detail/NativeFd.cpp \
  "$TDUTILS_DIR"/td/utils/port/detail/ThreadIdGuard.cpp \
  -I"$SCRIPT_DIR" -I"$TD_ROOT/td/generate/auto" -I"$TD_ROOT" -I"$TDUTILS_DIR"

echo "== building the Rust corpus dumps =="
env -u RUSTC_WRAPPER cargo build -q --release --manifest-path "$RS_ROOT/Cargo.toml" -p tdmtproto \
  --features conformance-samples --bin dump_wire_replay
env -u RUSTC_WRAPPER cargo build -q --release --manifest-path "$RS_ROOT/Cargo.toml" -p tde2e \
  --features conformance-samples --bin dump_e2e_replay
RUST_BIN="${CARGO_TARGET_DIR:-$RS_ROOT/target}/release/dump_wire_replay"
RUST_E2E_BIN="${CARGO_TARGET_DIR:-$RS_ROOT/target}/release/dump_e2e_replay"

"$RUST_BIN" >"$WORK/corpus.txt"
"$RUST_E2E_BIN" >>"$WORK/corpus.txt"

echo "== replaying $(wc -l <"$WORK/corpus.txt") samples through td's parsers =="
"$WORK/replay" <"$WORK/corpus.txt" | tee "$WORK/summary.txt"

grep -q 'failures=0' "$WORK/summary.txt"
echo "S15 verification OK"
