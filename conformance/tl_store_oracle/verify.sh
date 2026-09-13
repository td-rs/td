#!/usr/bin/env bash
# S10 verification: the tl-codegen-generated telegram_api binary store must
# produce bytes identical to td's own generated C++ store methods
# (TlStorerUnsafe) for a sample set covering every wire form — trivial ids,
# doubles, longs, TL string framing (incl. the 0xfe long form), vectors,
# the flags word with auto-OR'd ?true bools, conditional ints/objects/
# doubles, and boxed objects.
#
# The C++ oracle compiles td's checked-in generated telegram_api_*.cpp (the
# files td itself ships, regenerated from the pinned scheme commit) plus the
# minimal tdutils runtime, against the unmodified td sources.
#
# Usage: ./verify.sh [td-repo-root]   (default: ../../../td relative to here)
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
TD_ROOT=${1:-"$SCRIPT_DIR/../../../td"}
GENERATED_DIR="$TD_ROOT/td/generate/auto/td/telegram"
TDUTILS_DIR="$TD_ROOT/tdutils"
RS_ROOT="$SCRIPT_DIR/../.."
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

[ -f "$GENERATED_DIR/telegram_api_0.cpp" ] || {
  echo "not found: $GENERATED_DIR/telegram_api_0.cpp (pass the td repo root)" >&2
  exit 1
}

echo "== building the C++ oracle from td's generated telegram_api =="
# -w: generated code trips warnings; only behavior matters here.
# $SCRIPT_DIR first on the include path: it shadows td/utils/config.h with
# the minimal all-features-off stand-in (same trick as S04's cpp_dump).
# c++ (not cc): the driver and generated code are C++ and need libstdc++.
c++ -O1 -w -o "$WORK/dump_store" "$SCRIPT_DIR/dump_store.cpp" \
  "$GENERATED_DIR"/telegram_api_*.cpp \
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
  -I"$SCRIPT_DIR" -I"$GENERATED_DIR/../.." -I"$TD_ROOT" -I"$TDUTILS_DIR"

echo "== building the Rust dump-telegram-store =="
env -u RUSTC_WRAPPER cargo build -q --release --manifest-path "$RS_ROOT/Cargo.toml" \
  -p tdmtproto --bin dump_telegram_store
RUST_BIN="$RS_ROOT/target/release/dump_telegram_store"

"$WORK/dump_store" >"$WORK/c.txt"
"$RUST_BIN" >"$WORK/rs.txt"
if ! cmp -s "$WORK/c.txt" "$WORK/rs.txt"; then
  echo "MISMATCH:" >&2
  diff "$WORK/c.txt" "$WORK/rs.txt" | head -40 >&2
  exit 1
fi
printf 'OK  %d samples byte-identical\n' "$(wc -l <"$WORK/c.txt")"
echo "S10 verification OK"
