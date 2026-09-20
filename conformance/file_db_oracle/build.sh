#!/usr/bin/env bash
# M.12.1h FileDb oracle build: compiles probe.cpp against the pristine
# td sources (git HEAD tree for the FileLocation/FileData headers, the
# real FileEncryptionKey.cpp/FileType.cpp impls) with the same shadow
# config.h recipe as the M.5.4 DialogParticipant oracle. A single g++
# invocation with --gc-sections keeps only the reached template
# instantiations.
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

echo "== shadowing td/utils/config.h (openssl on) =="
mkdir -p "$WORK/shadow/td/utils"
cp "$SCRIPT_DIR/config.h" "$WORK/shadow/td/utils/config.h"

OPENSSL_DEV=${OPENSSL_DEV:-/nix/store/3zlj0508348qn8y0f45y7f7954amigpp-openssl-3.6.3-dev}
OPENSSL_OUT=${OPENSSL_OUT:-/nix/store/hg41dqig0qxm7shyi0a0xkw29a14i3js-openssl-3.6.3}
OPENSSL_DEV=${OPENSSL_DEV:-$(nix build --print-out-paths nixpkgs#openssl.dev 2>/dev/null | tail -1)}
OPENSSL_OUT=${OPENSSL_OUT:-$(nix build --print-out-paths nixpkgs#openssl 2>/dev/null | tail -1)}
ZLIB_OUT=${ZLIB_OUT:-/nix/store/l3p6y554mkxi4vipnnp8809kxj0g1lyv-zlib-1.3.2}

echo "== compiling the oracle and linking the TD_BUILD static libs =="
# FileData::parse instantiates the file-source registration plane even
# with register_file_sources=false (both template arms are emitted), so
# the manager symbols must resolve: link the full TD_BUILD archive set.
AR=$(ls -d "$TD_ROOT/TD_BUILD" 2>/dev/null || true)
if [ -z "$AR" ]; then
  echo "TD_BUILD static libs absent in $TD_ROOT — build the C++ td first" >&2
  exit 1
fi

c++ -std=c++17 -O2 -w -fno-exceptions -fno-rtti \
  -o "$SCRIPT_DIR/oracle" "$SCRIPT_DIR/probe.cpp" \
  "$TD_ROOT/tdutils/td/utils/Ed25519.cpp" \
  "$AR/libtdcore.a" \
  "$AR/libtdclient.a" \
  "$AR/libtdapi.a" \
  "$AR/libtdmtproto.a" \
  "$AR/tddb/libtddb.a" \
  "$AR/tdnet/libtdnet.a" \
  "$AR/tdactor/libtdactor.a" \
  "$AR/tdutils/libtdutils.a" \
  "$AR/tdtl/libtdtl.a" \
  "$AR/sqlite/libtdsqlite.a" \
  "$AR/tde2e/libtde2e.a" \
  -I"$WORK/shadow" -I"$TD_ROOT/td/generate/auto" -I"$WORK/tdnet" -I"$TD_ROOT" -I"$TD_ROOT/tdactor" -I"$TD_ROOT/tddb" -I"$TD_ROOT/sqlite" -I"$TD_ROOT/tdutils" \
  -I"$OPENSSL_DEV/include" \
  -pthread -ldl -L"$OPENSSL_OUT/lib" -L"$ZLIB_OUT/lib" -lssl -lcrypto -lz

echo "== running the oracle =="
"$SCRIPT_DIR/oracle" | tee "$SCRIPT_DIR/oracle.out"
wc -l < "$SCRIPT_DIR/oracle.out" | xargs printf 'oracle: %s lines\n'
