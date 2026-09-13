#!/usr/bin/env bash
# Regenerates td's gitignored td/generate/auto/ tree from the pinned td
# checkout by compiling td's own unmodified generators — tl-parser,
# generate_common, generate_mtproto, generate_json — the same pairings td's
# cmake targets tl_generate_tlo + tl_generate_common / tl_generate_mtproto /
# tl_generate_json run. td's repo does NOT check auto/ in (it is build
# output, .gitignore `**/auto/`); the S10/S12/S15 oracles compile files from
# it, so each calls this helper when its file is absent.
#
# Usage: ./generate.sh <td-repo-root> <work-dir>
set -euo pipefail

TD_ROOT=${1:?usage: generate.sh <td-repo-root> <work-dir>}
WORK=${2:?usage: generate.sh <td-repo-root> <work-dir>}
GEN_DIR="$TD_ROOT/td/generate"
TDTL_DIR="$TD_ROOT/tdtl"
TDUTILS_DIR="$TD_ROOT/tdutils"
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)

[ -f "$GEN_DIR/generate_common.cpp" ] || {
  echo "not found: $GEN_DIR/generate_common.cpp (pass the td repo root)" >&2
  exit 1
}

# td's own cmake pre-creates these output dirs (td/generate/CMakeLists.txt);
# tl-parser does not create them itself.
mkdir -p "$GEN_DIR/auto/tlo" "$GEN_DIR/auto/td/telegram" "$GEN_DIR/auto/td/mtproto"

# Same source set as td's tl_writer_cpp cmake target (TD_ENABLE_JNI off,
# as in td's default build).
WRITER_SRCS=(
  "$GEN_DIR/tl_writer_cpp.cpp" "$GEN_DIR/tl_writer_h.cpp" "$GEN_DIR/tl_writer_hpp.cpp"
  "$GEN_DIR/tl_writer_jni_cpp.cpp" "$GEN_DIR/tl_writer_jni_h.cpp" "$GEN_DIR/tl_writer_td.cpp"
)

echo "== building td's tl-parser and generating the .tlo files =="
cc -O2 -w -o "$WORK/tl-parser" \
  "$GEN_DIR/tl-parser/tl-parser.c" "$GEN_DIR/tl-parser/tlc.c" "$GEN_DIR/tl-parser/crc32.c" \
  -I"$GEN_DIR/tl-parser" -lm
(cd "$GEN_DIR" && for scheme in mtproto_api secret_api e2e_api td_api telegram_api; do
  "$WORK/tl-parser" -e "auto/tlo/$scheme.tlo" "scheme/$scheme.tl"
done)

echo "== building td's generate_common and generate_mtproto and running them =="
c++ -O2 -w -o "$WORK/generate_common" \
  "$GEN_DIR/generate_common.cpp" "${WRITER_SRCS[@]}" "$TDTL_DIR"/td/tl/*.cpp \
  -I"$GEN_DIR" -I"$TDTL_DIR"
c++ -O2 -w -o "$WORK/generate_mtproto" \
  "$GEN_DIR/generate_mtproto.cpp" "${WRITER_SRCS[@]}" "$TDTL_DIR"/td/tl/*.cpp \
  -I"$GEN_DIR" -I"$TDTL_DIR"
(cd "$GEN_DIR/auto" && "$WORK/generate_common" && "$WORK/generate_mtproto")

echo "== building td's generate_json and running it =="
# $SCRIPT_DIR first on the include path: it shadows tdutils' cmake-generated
# td/utils/config.h with the minimal all-features-off stand-in (same trick
# as the oracle drivers; the config is the only file cmake generates into
# tdutils, everything else compiles unmodified).
c++ -O2 -w -o "$WORK/generate_json" \
  "$GEN_DIR/generate_json.cpp" "$GEN_DIR/tl_json_converter.cpp" \
  "$TDTL_DIR"/td/tl/*.cpp \
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
  -I"$SCRIPT_DIR" -I"$GEN_DIR" -I"$TDTL_DIR" -I"$TDUTILS_DIR" -I"$TD_ROOT"
(cd "$GEN_DIR/auto" && "$WORK/generate_json")

echo "td auto sources generated"
