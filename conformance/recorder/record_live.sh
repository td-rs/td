#!/usr/bin/env bash
# The live test-DC recording driver (S75): builds the live request corpus
# from the TD_RECORD_* credentials and records the session with the C++
# recorder. The login code is pre-provisioned in TD_CODE (the operator reads
# the SMS from the test DC and stores it before the nightly run); the
# recorded corpus lands in corpus/live_requests.txt, and the session record
# (requests/updates/responses) in corpus/live_session.jsonl.
#
# Usage: ./record_live.sh [td-repo-root]
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
TD_ROOT=${1:-"$SCRIPT_DIR/../../../td"}
TD_BUILD="${TD_BUILD_DIR:-$TD_ROOT/TD_BUILD}"
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

: "${TD_API_ID:?test-DC api_id missing}"
: "${TD_API_HASH:?test-DC api_hash missing}"
: "${TD_PHONE_NUMBER:?test-DC phone number missing}"
: "${TD_CODE:?login code missing (read the SMS before the nightly run)}"

mkdir -p "$SCRIPT_DIR/corpus"

cat >"$SCRIPT_DIR/corpus/live_requests.txt" <<EOF
{"@type":"setTdlibParameters","api_id":${TD_API_ID},"api_hash":"${TD_API_HASH}","system_language_code":"en","device_model":"td-rs recorder","system_version":"CI","application_version":"1.0","use_test_dc":true,"database_directory":"$WORK/tdlib","files_directory":"$WORK/files"}
{"@type":"setAuthenticationPhoneNumber","phone_number":"${TD_PHONE_NUMBER}"}
{"@type":"checkAuthenticationCode","code":"${TD_CODE}"}
{"@type":"getMe"}
EOF

echo "== building the C++ recorder against libtdjson =="
c++ -O1 -o "$WORK/record" "$SCRIPT_DIR/cpp/record.cpp" \
  -I"$TD_BUILD" -I"$TD_ROOT" -L"$TD_BUILD" -ltdjson

LD_LIBRARY_PATH="$TD_BUILD" "$WORK/record" <"$SCRIPT_DIR/corpus/live_requests.txt" \
  >"$SCRIPT_DIR/corpus/live_session.jsonl" 2>/dev/null

printf 'OK  recorded %d lines\n' "$(wc -l <"$SCRIPT_DIR/corpus/live_session.jsonl")"
