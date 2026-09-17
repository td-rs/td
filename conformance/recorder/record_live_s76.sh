#!/usr/bin/env bash
# The S76 walking-skeleton recording driver: records a live test-DC session
# as the harness bot (TD_BOT_TOKEN, created once with the S66 `bot_maker`
# tool) — the bot-token login, getMe, a Saved-Messages text sendMessage and
# the received echo, through the C++ recorder. Bot tokens are reusable —
# unlike phone codes — so the Rust replayer runs the same corpus live and
# the two streams diff modulo the documented nondeterminism.
#
# Usage: ./record_live_s76.sh [td-repo-root]
# Env: TD_BOT_TOKEN (required), TD_API_ID/TD_API_HASH (default: the tdlib
# test pair 94575), TD_RECORD_LINGER_MS (default 5000 — the echo window).
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
TD_ROOT=${1:-"$SCRIPT_DIR/../../../td"}
TD_BUILD="${TD_BUILD_DIR:-$TD_ROOT/TD_BUILD}"
WORK="$SCRIPT_DIR/corpus/s76_work"
LINGER="${TD_RECORD_LINGER_MS:-5000}"

: "${TD_BOT_TOKEN:?test-DC bot token missing (create one with the bot_maker tool)}"
: "${TD_API_HASH:?test-DC api_hash missing}"

mkdir -p "$SCRIPT_DIR/corpus"
rm -rf "$WORK"
"$SCRIPT_DIR/s76_corpus_gen.sh" "$WORK" "$SCRIPT_DIR/corpus/s76_requests.txt" >/dev/null

echo "== building the C++ recorder against libtdjson =="
WORK_BIN=$(mktemp -d)
trap 'rm -rf "$WORK_BIN"' EXIT
c++ -O1 -o "$WORK_BIN/record" "$SCRIPT_DIR/cpp/record.cpp" \
  -I"$TD_BUILD" -I"$TD_ROOT" -L"$TD_BUILD" -ltdjson

LD_LIBRARY_PATH="$TD_BUILD" TD_RECORD_LINGER_MS="$LINGER" "$WORK_BIN/record" \
  <"$SCRIPT_DIR/corpus/s76_requests.txt" \
  >"$SCRIPT_DIR/corpus/s76_session.jsonl" 2>/dev/null

printf 'OK  recorded %d lines\n' "$(wc -l <"$SCRIPT_DIR/corpus/s76_session.jsonl")"
