#!/usr/bin/env bash
# S82 packaging: assemble a tdlib-release-layout artifact from the Rust cdylib.
# Layout mirrors the official tdlib binary zip (include/ + lib/, versioned
# soname + symlink) — the JSON-interface install set of the C++ build:
#   include/td/telegram/{td_json_client.h,td_log.h,tdjson_export.h}
#   lib/libtdjson.so.1.8.67 (SONAME patched) + lib/libtdjson.so -> it
# Usage: make-release.sh <release-cdylib> <out-dir>
set -euo pipefail
SRC_SO="$1"
OUT="$2"
VER=1.8.67
TD_CPP=${TD_CPP:-/home/sadiq/Projects/td}
GEN_EXPORT=${GEN_EXPORT:-$TD_CPP/build-d1/td/telegram/tdjson_export.h}

mkdir -p "$OUT/include/td/telegram" "$OUT/lib"
install -m 644 "$TD_CPP/td/telegram/td_json_client.h" "$OUT/include/td/telegram/"
install -m 644 "$TD_CPP/td/telegram/td_log.h" "$OUT/include/td/telegram/"
install -m 644 "$GEN_EXPORT" "$OUT/include/td/telegram/tdjson_export.h"

install -m 755 "$SRC_SO" "$OUT/lib/libtdjson.so.$VER"
patchelf --set-soname "libtdjson.so.$VER" "$OUT/lib/libtdjson.so.$VER"
ln -sf "libtdjson.so.$VER" "$OUT/lib/libtdjson.so"
echo "PACKAGED_OK $OUT"
