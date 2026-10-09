#!/usr/bin/env bash
# S83 parity checks (macOS): Rust release artifact vs a C++ reference build
# ON THE SAME MAC. 1. Exported symbol sets identical (14-symbol ABI)
# 2. Shipped headers byte-identical to the C++ tree
# 3. Artifact layout matches the tdlib release set (install-id + symlink)
#
# ⚠ UNTESTED-ON-MAC (written from the linux twin on 2026-09-28; the
# symbol/id probes use nm -gU / otool — the macOS spellings of
# nm -D --defined-only / readelf soname).
# Usage: parity-check.sh <dist-dir>   (TD_CPP = the C++ checkout with a
# cmake build; CPP_BUILD = that build dir, default first found)
set -euo pipefail
DIST="${1:?dist dir}"
TD_CPP=${TD_CPP:-$HOME/Projects/td}
CPP_BUILD=${CPP_BUILD:-$(find "$TD_CPP" -name libtdjson.dylib -path '*build*' -exec dirname {} \; | head -1)}
[ -n "$CPP_BUILD" ] || { echo "C++ reference dylib not found under $TD_CPP (cmake-build the tree first)"; exit 1; }
CPP_SO="$CPP_BUILD/libtdjson.dylib"

fail=0
# 1. symbol sets — global defined, text symbols only (the 14 td_* C ABI)
nm -gU "$CPP_SO" | awk '$2=="T"{print $1}' | sort > /tmp/parity-cpp-mac.txt
nm -gU "$DIST/lib/libtdjson.dylib" | awk '$2=="T"{print $1}' | sort > /tmp/parity-rs-mac.txt
if diff -u /tmp/parity-cpp-mac.txt /tmp/parity-rs-mac.txt; then echo "PARITY_SYMBOLS_OK ($(wc -l < /tmp/parity-rs-mac.txt) exported)"; else echo "PARITY_SYMBOLS_DIFF"; fail=1; fi

# 2. headers
for h in td_json_client.h td_log.h; do
  if cmp -s "$TD_CPP/td/telegram/$h" "$DIST/include/td/telegram/$h"; then echo "PARITY_HEADER_OK $h"; else echo "PARITY_HEADER_DIFF $h"; fail=1; fi
done
GEN_EXPORT=$(find "$TD_CPP" -name tdjson_export.h -path '*build*' | head -1)
if cmp -s "$GEN_EXPORT" "$DIST/include/td/telegram/tdjson_export.h"; then echo "PARITY_HEADER_OK tdjson_export.h"; else echo "PARITY_HEADER_DIFF tdjson_export.h"; fail=1; fi

# 3. layout + install-id + symlink
ID=$(otool -D "$DIST/lib/libtdjson.dylib" | tail -1 | sed 's@.*/@@')
[ "$ID" = "libtdjson.1.8.68.dylib" ] && echo "PARITY_INSTALL_ID_OK $ID" || { echo "PARITY_INSTALL_ID_BAD '$ID'"; fail=1; }
[ "$(readlink "$DIST/lib/libtdjson.dylib")" = "libtdjson.1.8.68.dylib" ] && echo "PARITY_SYMLINK_OK" || { echo "PARITY_SYMLINK_BAD"; fail=1; }

exit $fail
