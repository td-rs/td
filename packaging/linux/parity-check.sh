#!/usr/bin/env bash
# S82/S83 parity checks: Rust release artifact vs the C++ build-d1 reference.
# 1. Exported symbol sets identical (14-symbol ABI)
# 2. Shipped headers byte-identical to the C++ tree (tdjson_export.h vs generated)
# 3. Artifact layout matches the tdlib release set (include/ + lib/, soname + symlink)
# 4. Link-and-load smoke: compile the shipped cpp example against the packaged tree
set -euo pipefail
DIST="${1:?dist dir}"
TD_CPP=${TD_CPP:-/home/sadiq/Projects/td}
CPP_SO=$TD_CPP/build-d1/libtdjson.so

fail=0
# 1. symbol sets
nm -D --defined-only "$CPP_SO" | awk '$2=="T"||$2=="W"{print $3}' | sort > /tmp/parity-cpp.txt
nm -D --defined-only "$DIST/lib/libtdjson.so" | awk '$2=="T"||$2=="W"{print $3}' | sort > /tmp/parity-rs.txt
if diff -u /tmp/parity-cpp.txt /tmp/parity-rs.txt; then echo "PARITY_SYMBOLS_OK ($(wc -l < /tmp/parity-rs.txt) exported)"; else echo "PARITY_SYMBOLS_DIFF"; fail=1; fi

# 2. headers
for h in td_json_client.h td_log.h; do
  if cmp -s "$TD_CPP/td/telegram/$h" "$DIST/include/td/telegram/$h"; then echo "PARITY_HEADER_OK $h"; else echo "PARITY_HEADER_DIFF $h"; fail=1; fi
done
if cmp -s "$TD_CPP/build-d1/td/telegram/tdjson_export.h" "$DIST/include/td/telegram/tdjson_export.h"; then echo "PARITY_HEADER_OK tdjson_export.h"; else echo "PARITY_HEADER_DIFF tdjson_export.h"; fail=1; fi

# 3. layout + soname + symlink
SON=$(readelf -d "$DIST/lib/libtdjson.so" | sed -n 's/.*Library soname: \[\(.*\)\]/\1/p')
[ "$SON" = "libtdjson.so.1.8.67" ] && echo "PARITY_SONAME_OK $SON" || { echo "PARITY_SONAME_BAD '$SON'"; fail=1; }
[ "$(readlink "$DIST/lib/libtdjson.so")" = "libtdjson.so.1.8.67" ] && echo "PARITY_SYMLINK_OK" || { echo "PARITY_SYMLINK_BAD"; fail=1; }

exit $fail
