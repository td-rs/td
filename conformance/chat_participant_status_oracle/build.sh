#!/usr/bin/env bash
# M.5.3 C++ oracle build: compiles probe.cpp + td's real DialogParticipant.cpp /
# telegram misc.cpp against a PRISTINE tdutils+tdnet tree extracted from git
# HEAD (the shared td worktree may carry unrelated edits from parallel
# sessions; pristine copies keep the oracle independent of them). Generated tl
# headers come from td/generate/auto (gitignored build output). A single g++
# invocation: one process, minimal CPU/RAM.
#
# Usage: ./build.sh [td-repo-root]   (default: ../../../td relative to here)
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
TD_ROOT=${1:-"$SCRIPT_DIR/../../../td"}
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

if [ ! -f "$TD_ROOT/td/generate/auto/td/telegram/telegram_api.h" ]; then
  echo "generated tl headers absent in $TD_ROOT/td/generate/auto — run conformance/generate_td_auto first" >&2
  exit 1
fi

echo "== extracting pristine tdutils + tdnet from git HEAD =="
git -C "$TD_ROOT" archive HEAD tdutils tdnet | tar -x -C "$WORK"

echo "== shadowing td/utils/config.h (openssl on; the telegram misc.cpp md5 needs it) =="
mkdir -p "$WORK/shadow/td/utils"
cp "$SCRIPT_DIR/config.h" "$WORK/shadow/td/utils/config.h"

OPENSSL_DEV=${OPENSSL_DEV:-/nix/store/3zlj0508348qn8y0f45y7f7954amigpp-openssl-3.6.3-dev}
OPENSSL_OUT=${OPENSSL_OUT:-/nix/store/hg41dqig0qxm7shyi0a0xkw29a14i3js-openssl-3.6.3}

TDUTILS_SRC="$WORK/tdutils/td/utils"
TDUTILS_ROOT="$WORK/tdutils"

echo "== compiling the oracle (single TU set, -O2, --gc-sections drops the td_api/telegram_api builders) =="
c++ -std=c++17 -O2 -w -fno-exceptions -fno-rtti -ffunction-sections -fdata-sections \
  -o "$SCRIPT_DIR/oracle" "$SCRIPT_DIR/probe.cpp" \
  "$TD_ROOT/td/telegram/DialogParticipant.cpp" \
  "$TD_ROOT/td/telegram/misc.cpp" \
  "$TDUTILS_SRC/logging.cpp" \
  "$TDUTILS_SRC/Time.cpp" \
  "$TDUTILS_SRC/Status.cpp" \
  "$TDUTILS_SRC/TsCerr.cpp" \
  "$TDUTILS_SRC/TsLog.cpp" \
  "$TDUTILS_SRC/ExitGuard.cpp" \
  "$TDUTILS_SRC/FlatHashTable.cpp" \
  "$TDUTILS_SRC/filesystem.cpp" \
  "$TDUTILS_SRC/PathView.cpp" \
  "$TDUTILS_SRC/Random.cpp" \
  "$TDUTILS_SRC/crypto.cpp" \
  "$TDUTILS_SRC/StackAllocator.cpp" \
  "$TDUTILS_SRC/tl_parsers.cpp" \
  "$TDUTILS_SRC/check.cpp" \
  "$TDUTILS_SRC/StringBuilder.cpp" \
  "$TDUTILS_SRC/Slice.cpp" \
  "$TDUTILS_SRC/buffer.cpp" \
  "$TDUTILS_SRC/misc.cpp" \
  "$TDUTILS_SRC/utf8.cpp" \
  "$TDUTILS_SRC/unicode.cpp" \
  "$TDUTILS_SRC/port/"*.cpp \
  "$TDUTILS_SRC/port/detail/NativeFd.cpp" \
  "$TDUTILS_SRC/port/detail/ThreadIdGuard.cpp" \
  -I"$WORK/shadow" -I"$TD_ROOT/td/generate/auto" -I"$WORK/tdnet" -I"$TD_ROOT" -I"$TD_ROOT/tdactor" -I"$TD_ROOT/tddb" -I"$TD_ROOT/sqlite" -I"$TDUTILS_ROOT" \
  -I"$OPENSSL_DEV/include" \
  -Wl,--gc-sections -pthread -ldl -L"$OPENSSL_OUT/lib" -lcrypto

echo "== running the oracle =="
"$SCRIPT_DIR/oracle" > "$SCRIPT_DIR/oracle.out"
wc -l < "$SCRIPT_DIR/oracle.out" | xargs printf 'oracle: %s lines\n'
