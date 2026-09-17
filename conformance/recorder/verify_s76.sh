#!/usr/bin/env bash
# The S76 gate: the walking-skeleton diff, both directions. Each side runs
# live against the test DC: the C++ recorder and the Rust replayer both
# consume the same corpus — bot-token login, getMe, the RECEIVE of a
# message from the harness phone account (TD_PEER_SENDER fires it while the
# recorder sits in the `updateNewMessage` wait — bot messages are delivered
# to open sessions only), and a SEND of the reply — then the two streams
# diff modulo the documented nondeterminism (times, message ids/dates).
#
# Usage: ./verify_s76.sh [td-repo-root]
# Env: TD_BOT_TOKEN, TD_BOT_USERNAME, TD_API_HASH (required),
# TD_PEER_USER_ID (default 2202815082), TD_PEER_SENDER (path to the
# bot_maker binary; the phone-account sender over the S66 session),
# TD_PEER_STATE (path to the phone-account persisted state,
# default /tmp/s66_state.bin), TD_RECORD_LINGER_MS (default 8000).
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
TD_ROOT=${1:-"$SCRIPT_DIR/../../../td"}
TD_BUILD="${TD_BUILD_DIR:-$TD_ROOT/TD_BUILD}"
RS_ROOT="$SCRIPT_DIR/../.."
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
LINGER="${TD_RECORD_LINGER_MS:-8000}"

: "${TD_BOT_TOKEN:?test-DC bot token missing (create one with the bot_maker tool)}"
: "${TD_BOT_USERNAME:?test-DC bot username missing}"
: "${TD_API_HASH:?test-DC api_hash missing}"
: "${TD_PEER_USER_ID:=2202815082}"
: "${TD_PEER_STATE:=/tmp/s66_state.bin}"

CORPUS="$("$SCRIPT_DIR/s76_corpus_gen.sh" "$WORK")"

echo "== building the C++ recorder against libtdjson =="
c++ -O1 -o "$WORK/record" "$SCRIPT_DIR/cpp/record.cpp" \
  -I"$TD_BUILD" -I"$TD_ROOT" -L"$TD_BUILD" -ltdjson

echo "== building the Rust replayer =="
env -u RUSTC_WRAPPER cargo build -q \
  --manifest-path "$RS_ROOT/conformance/Cargo.toml" --bin recorder-replayer
RUST_BIN="${CARGO_TARGET_DIR:-$RS_ROOT/conformance/target}/debug/recorder-replayer"

echo "== building the phone-account sender (bot_maker) =="
env -u RUSTC_WRAPPER cargo build -q \
  --manifest-path "$RS_ROOT/conformance/Cargo.toml" --bin bot_maker
PEER_SENDER="${TD_PEER_SENDER:-${CARGO_TARGET_DIR:-$RS_ROOT/conformance/target}/debug/bot_maker}"

# Runs `side` on the corpus; the phone-account message fires while the run
# waits for `updateNewMessage` (bot messages reach open sessions only).
run_side() {
  local side=$1
  local out=$2
  rm -rf "$WORK/tdlib" "$WORK/files"
  local driver
  if [ "$side" = cpp ]; then
    driver=("$WORK/record")
  else
    driver=("$RUST_BIN")
  fi
  stdbuf -oL env LD_LIBRARY_PATH="$TD_BUILD" TD_RECORD_LINGER_MS="$LINGER" \
    "${driver[@]}" <"$CORPUS" >"$out" 2>"$WORK/$side.err" &
  local pid=$!
  # Wait for the updateNewMessage wait, then fire the phone message.
  for _ in $(seq 1 240); do
    grep -q '"@wait":"updateNewMessage"' "$out" 2>/dev/null && break
    kill -0 "$pid" 2>/dev/null || break
    sleep 0.5
  done
  sleep 1
  "$PEER_SENDER" "$TD_PEER_STATE" --send "$TD_BOT_USERNAME" \
    "s76 hello from the phone" >/dev/null 2>&1 || true
  if ! wait "$pid"; then
    echo "side $side failed: rc=$?" >&2
    tail -25 "$WORK/$side.err" >&2 || true
    return 1
  fi
}

echo "== recording the live C++ session =="
run_side cpp "$WORK/c.txt"

echo "== replaying through the Rust client =="
run_side rs "$WORK/rs.txt"

# The S76 normalization: the two runs are separate live sessions, so the
# timestamps and the server-assigned message ids/dates differ; the bot
# token makes both logins real and repeatable. The commit_hash interim
# normalization rides along from the offline gate.
normalize() {
  # The masker runs as `python3 -c "$(cat <<'PY' ...)"` on purpose: a
  # `sed | python3 - <<'PY'` pipeline would hand the heredoc (not the
  # stream) to python's stdin — sed dies of SIGPIPE and, under
  # pipefail, the script exits 141 with no message.
  python3 -c "$(cat <<'PY'
import json
import re
import sys

work = sys.argv[1]

# The S76 normalization: the two runs are separate live sessions, so the
# timestamps and the server-assigned message ids/dates differ; the bot
# token makes both logins real and repeatable. The commit_hash interim
# normalization rides along from the offline gate.
COMMIT_HASH_RE = re.compile(
    r'("commit_hash","value":\{"@type":"optionValueString","value":")[^"]*"'
)
MASKED_OPTION_VALUES = {"authorization_date", "utc_time_offset"}
MASKED_KEYS = {
    "last_read_inbox_message_id",
    "last_read_outbox_message_id",
    "old_message_id",
}


def mask(obj):
    if isinstance(obj, dict):
        obj = dict(obj)
        type_name = obj.get("@type")
        if type_name == "message":
            obj["id"] = "<id>"
            obj["date"] = "<date>"
        if type_name == "updateOption":
            if obj.get("name") == "unix_time":
                return None  # the periodic broadcast — dropped on both sides
            if obj.get("name") in MASKED_OPTION_VALUES:
                obj["value"] = "<value>"
        for key in MASKED_KEYS:
            if key in obj:
                obj[key] = "<id>"
        return {k: v for k, v in ((k, mask(v)) for k, v in obj.items()) if v is not None}
    if isinstance(obj, list):
        return [v for v in (mask(v) for v in obj) if v is not None]
    return obj


for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    line = COMMIT_HASH_RE.sub('\\1<git>"', line)
    record = json.loads(line)
    # The `@wait` corpus markers are harness bookkeeping (the driver's
    # wait-for-update primitive, and how the gate times the phone
    # message) — their stream position races with the live updates, and
    # the diff is about client-emitted records. The request value is the
    # parsed JSON object, so the check is a dict-key test (a quoted
    # '"@wait"' string match never fires — the marker survived into the
    # diff streams until this fix).
    if "@wait" in record.get("request", {}):
        continue
    masked = mask(record)
    if not masked:
        continue
    out = {}
    for kind in ("request", "update", "response"):
        if kind in record:
            out[kind] = json.dumps(
                masked[kind], separators=(",", ":"), ensure_ascii=False
            )
    print(json.dumps(out, separators=(",", ":"), ensure_ascii=False))
PY
)" "$WORK"
}

normalize <"$WORK/c.txt" >"$WORK/c.norm"
normalize <"$WORK/rs.txt" >"$WORK/rs.norm"

if ! cmp -s "$WORK/c.norm" "$WORK/rs.norm"; then
  echo "MISMATCH:" >&2
  diff "$WORK/c.norm" "$WORK/rs.norm" | head -80 >&2
  exit 1
fi
printf 'OK  %d records byte-identical modulo the S76 nondeterminism\n' \
  "$(wc -l <"$WORK/c.norm")"
echo "S76 verification OK"
