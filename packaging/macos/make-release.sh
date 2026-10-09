#!/usr/bin/env bash
# S82/S83 packaging (macOS): assemble a tdlib-release-layout artifact from
# the Rust cdylib. Layout mirrors the official tdlib binary zip for macOS —
# the JSON-interface install set of the C++ build:
#   include/td/telegram/{td_json_client.h, td_log.h, tdjson_export.h}
#   lib/libtdjson.1.8.68.dylib (install-id patched) + lib/libtdjson.dylib -> it
#
# ⚠ UNTESTED-ON-MAC (written from the linux twin on 2026-09-28 — the S83
# audit documents macOS as the toolchain gap; this script exists so the
# artifact is ONE COMMAND once an Apple toolchain is available). The only
# Linux-shaped step (patchelf --set-soname) becomes install_name_tool -id;
# everything else is plain file assembly. Run it ON the Mac after:
#   cargo build --release --locked -p tdjson
# with TD_CPP pointing at a C++ tdlib checkout that has a configured cmake
# build (its cmake-generated tdjson_export.h ships in the package).
set -euo pipefail
SRC_DYLIB="${1:?usage: make-release.sh <release-cdylib> <out-dir>}"
OUT="${2:?usage: make-release.sh <release-cdylib> <out-dir>}"
VER=1.8.68
TD_CPP=${TD_CPP:-$HOME/Projects/td}
# The cmake build dir carrying the generated tdjson_export.h (any configured
# build of the pinned tree — the header is target-canonical for macOS).
GEN_EXPORT=${GEN_EXPORT:-$(find "$TD_CPP" -name tdjson_export.h -path '*build*' | head -1)}
[ -n "$GEN_EXPORT" ] || { echo "tdjson_export.h not found under $TD_CPP (build the C++ tree first)"; exit 1; }

mkdir -p "$OUT/include/td/telegram" "$OUT/lib"
install -m 644 "$TD_CPP/td/telegram/td_json_client.h" "$OUT/include/td/telegram/"
install -m 644 "$TD_CPP/td/telegram/td_log.h" "$OUT/include/td/telegram/"
install -m 644 "$GEN_EXPORT" "$OUT/include/td/telegram/tdjson_export.h"

install -m 755 "$SRC_DYLIB" "$OUT/lib/libtdjson.$VER.dylib"
# The macOS soname equivalent: the install-id clients record at link time
# (what otool -D reports). Like the linux twin, keep it bare so consumers
# resolve through their rpath.
install_name_tool -id "libtdjson.$VER.dylib" "$OUT/lib/libtdjson.$VER.dylib"
ln -sf "libtdjson.$VER.dylib" "$OUT/lib/libtdjson.dylib"
codesign --force --sign - "$OUT/lib/libtdjson.$VER.dylib" >/dev/null 2>&1 || \
  echo "(codesign ad-hoc re-sign skipped/failed — install_name_tool invalidated the original signature)"
echo "PACKAGED_OK $OUT"
