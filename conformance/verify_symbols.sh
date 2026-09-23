#!/usr/bin/env bash
# S83 drop-in surface audit: the built tdjson cdylib must export every
# symbol of the C++ `tdclientjson_export_list` (the 14-symbol C ABI, D3/D6)
# with nothing beyond it but the ELF/compiler-runtime necessities.
#
# A drop-in replacement has to keep the 14 present (a binding that resolves
# any of them fails to load otherwise); the C++ .so itself is not
# symbol-filtered on ELF (the exported_symbols_list is Darwin-only), so
# "no fewer" is the hard drop-in direction and "no extra library API" is the
# S83 nothing-beyond-tdlib direction — the Rust cdylib exports exactly the
# 14 #[no_mangle] functions.
#
# Usage: ./verify_symbols.sh [path/to/libtdjson.so]
#   (default: target/release/libtdjson.so; build with
#    `cargo build --release -p tdjson`)
set -euo pipefail

SO=${1:-target/release/libtdjson.so}
if [ ! -f "$SO" ]; then
  echo "FAIL: $SO not found (build it: cargo build --release -p tdjson)" >&2
  exit 1
fi

# The pinned C++ export list — tdclientjson_export_list at the S03 scheme
# pin d1085f9cebc5a62379991ae1652673954f229c1f (Darwin ld64 format, one
# underscore-prefixed symbol per line). Order-insensitive here: drop-in is
# about presence, the ORDER is pinned separately by the
# tdjson::tests::export_list_is_the_cpp_one unit test.
EXPECTED="td_json_client_create
td_json_client_destroy
td_json_client_send
td_json_client_receive
td_json_client_execute
td_set_log_file_path
td_set_log_max_file_size
td_set_log_verbosity_level
td_set_log_fatal_error_callback
td_create_client_id
td_send
td_receive
td_execute
td_set_log_message_callback"

EXPECTED_COUNT=$(printf '%s\n' "$EXPECTED" | wc -l | tr -d ' ')
ACTUAL=$(nm -D --defined-only "$SO" | awk '{print $3}' | sort -u)

rc=0
echo "== required symbols (the C++ tdclientjson_export_list) =="
missing=0
for s in $EXPECTED; do
  if printf '%s\n' "$ACTUAL" | grep -qx "$s"; then
    echo "  ok        $s"
  else
    echo "  MISSING   $s"
    missing=1
  fi
done
if [ "$missing" -ne 0 ]; then
  echo "FAIL: drop-in export surface incomplete"
  rc=1
fi

echo "== extra exported symbols beyond the 14 =="
# ELF/compiler-runtime housekeeping that every shared object carries is not
# library API. Anything else must be called out in the S83 audit.
ALLOWLIST='^(_init|_fini|__bss_start|_edata|_end|__data_start|_DYNAMIC|__gmon_start__|__GNU_EH_FRAME_HDR|_GLOBAL_OFFSET_TABLE_|__cxa_(finalize|atexit)|_ITM_(register|deregister)TM(Clone|Table)|environ)$'
extras=$(printf '%s\n' "$ACTUAL" | grep -v -E "$ALLOWLIST" | grep -vx -f <(printf '%s\n' "$EXPECTED") || true)
if [ -n "$extras" ]; then
  echo "$extras" | sed 's/^/  extra    /'
  echo "NOTE: extras above are not in the C++ export list — document each in the S83 audit"
else
  echo "  (none — the cdylib ships exactly the 14-symbol ABI)"
fi

actual_count=$(printf '%s\n' "$ACTUAL" | wc -l | tr -d ' ')
echo "== $SO: $EXPECTED_COUNT required, $actual_count exported total =="
exit "$rc"
