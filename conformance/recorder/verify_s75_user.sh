#!/usr/bin/env bash
# The S75 user-path gate: a live test-DC PHONE login (not the S76 bot
# token) recorded through the C++ td_json_client and replayed through the
# Rust tdclient, diffed modulo the documented nondeterminism.
#
# The login code needs no operator: a tdlib phone login delivers its code
# to the account's active sessions as a 777000 "Login code" message, and
# the harness phone account's own authorized session (TD_PEER_STATE, the
# persisted S66 state) receives that push — `bot_maker --wait-code`
# listens for it and feeds the corpus live (codes are single-use, so each
# side burns its own; the checkAuthenticationCode value is masked in the
# diff). The corpus: setTdlibParameters → setAuthenticationPhoneNumber →
# checkAuthenticationCode → getMe. The account carries a cloud password
# (HAS_PASSWORD=true, no hint — checked 2026-09-28), so without
# TD_PASSWORD the session ends at authorizationStateWaitPassword and getMe
# answers pre-auth; with TD_PASSWORD the corpus completes the
# authorization first. Either way both sides walk the identical path.
#
# Usage: ./verify_s75_user.sh [td-repo-root]
# Env: TD_API_ID, TD_API_HASH, TD_PHONE_NUMBER (required);
# TD_PEER_STATE (default /tmp/s66_state.bin), TD_PASSWORD (optional),
# TD_RECORD_LINGER_MS (default 8000), TD_BUILD_DIR (C++ tdjson build).
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
TD_ROOT=${1:-"$SCRIPT_DIR/../../../td"}
TD_BUILD="${TD_BUILD_DIR:-$TD_ROOT/TD_BUILD}"
RS_ROOT="$SCRIPT_DIR/../.."
WORK=$(mktemp -d)
if [ -z "${S75_KEEP_LOGS:-}" ]; then
  trap 'rm -rf "$WORK"' EXIT
else
  echo "(S75_KEEP_LOGS=1 — workdir kept: $WORK)"
fi
LINGER="${TD_RECORD_LINGER_MS:-8000}"

: "${TD_API_ID:?test-DC api_id missing}"
: "${TD_API_HASH:?test-DC api_hash missing}"
: "${TD_PHONE_NUMBER:?test-DC phone number missing}"
: "${TD_PEER_STATE:=/tmp/s66_state.bin}"

echo "== building the C++ recorder against libtdjson =="
c++ -O1 -o "$WORK/record" "$SCRIPT_DIR/cpp/record.cpp" \
  -I"$TD_BUILD" -I"$TD_ROOT" -L"$TD_BUILD" -ltdjson

echo "== building the Rust replayer + the code listener =="
env -u RUSTC_WRAPPER cargo build -q \
  --manifest-path "$RS_ROOT/conformance/Cargo.toml" \
  --bin recorder-replayer --bin bot_maker
TARGET="${CARGO_TARGET_DIR:-$RS_ROOT/conformance/target}/debug"
RUST_BIN="$TARGET/recorder-replayer"
CODE_LISTENER="$TARGET/bot_maker"

# Records one side live. The code listener (the S66 session) must be up
# BEFORE the phone request fires — the 777000 push only reaches sessions
# that are online when the server sends it — so: start the listener, wait
# for its connected marker, then drive the login through the FIFO.
run_side() {
  local side=$1
  local out=$2
  local fifo="$WORK/$side.in"
  local dbdir="$WORK/$side-tdlib"
  local filesdir="$WORK/$side-files"
  rm -rf "$dbdir" "$filesdir"
  mkdir -p "$WORK/$side.d"
  mkfifo "$fifo"

  # The listener: prints LOGIN_CODE=<n> on stdout when the push arrives.
  "$CODE_LISTENER" "$TD_PEER_STATE" --wait-code --budget 300 \
    >"$WORK/$side.code" 2>"$WORK/$side.code.err" &
  local listener_pid=$!
  for _ in $(seq 1 60); do
    grep -q "listening for the login-code push" "$WORK/$side.code.err" \
      2>/dev/null && break
    kill -0 "$listener_pid" 2>/dev/null || break
    sleep 0.5
  done
  kill -0 "$listener_pid" 2>/dev/null || {
    echo "side $side: the code listener died:" >&2
    tail -5 "$WORK/$side.code.err" >&2
    return 1
  }

  local driver
  if [ "$side" = cpp ]; then
    driver=("$WORK/record")
  else
    driver=("$RUST_BIN")
  fi
  stdbuf -oL env LD_LIBRARY_PATH="$TD_BUILD" TD_RECORD_LINGER_MS="$LINGER" \
    "${driver[@]}" <"$fifo" >"$out" 2>"$WORK/$side.err" &
  local driver_pid=$!
  exec 3>"$fifo"

  printf '%s\n' "{\"@type\":\"setTdlibParameters\",\"api_id\":${TD_API_ID},\"api_hash\":\"${TD_API_HASH}\",\"system_language_code\":\"en\",\"device_model\":\"td-rs recorder\",\"system_version\":\"CI\",\"application_version\":\"1.0\",\"use_test_dc\":true,\"database_directory\":\"$dbdir\",\"files_directory\":\"$filesdir\"}" >&3
  # The config fan-out (getConfig/getAppConfig application — the M.3
  # acceptance surface) fires asynchronously after the parameters; the
  # idle window pins it into ONE stable stream position on both sides
  # instead of racing across the later request drains.
  printf '%s\n' '{"@idle":10}' >&3
  printf '%s\n' "{\"@type\":\"setAuthenticationPhoneNumber\",\"phone_number\":\"${TD_PHONE_NUMBER}\"}" >&3
  printf '%s\n' '{"@wait":"authorizationStateWaitCode"}' >&3

  # The code lands on the listener while the driver sits in the wait.
  local code=""
  for _ in $(seq 1 300); do
    code=$(sed -n 's/^LOGIN_CODE=//p' "$WORK/$side.code" | tail -1)
    [ -n "$code" ] && break
    kill -0 "$driver_pid" 2>/dev/null || break
    kill -0 "$listener_pid" 2>/dev/null || break
    sleep 1
  done
  [ -n "$code" ] || {
    echo "side $side: no login code arrived" >&2
    tail -5 "$WORK/$side.code.err" >&2
    return 1
  }
  printf '%s\n' "{\"@type\":\"checkAuthenticationCode\",\"code\":\"$code\"}" >&3
  if [ -n "${TD_PASSWORD:-}" ]; then
    printf '%s\n' '{"@wait":"authorizationStateWaitPassword"}' >&3
    printf '%s\n' "{\"@type\":\"checkAuthenticationPassword\",\"password\":\"${TD_PASSWORD}\"}" >&3
    printf '%s\n' '{"@wait":"authorizationStateReady"}' >&3
  else
    printf '%s\n' '{"@wait":"authorizationStateWaitPassword"}' >&3
  fi
  printf '%s\n' '{"@type":"getMe"}' >&3

  exec 3>&-
  wait "$listener_pid" 2>/dev/null || true
  if ! wait "$driver_pid"; then
    echo "side $side failed: rc=$?" >&2
    tail -25 "$WORK/$side.err" >&2
    return 1
  fi
}

echo "== recording the live C++ session (user path) =="
run_side cpp "$WORK/c.txt"

echo "== replaying the user path through the Rust client =="
run_side rs "$WORK/rs.txt"

# The S75 user-path normalization: everything the S76 gate masks (times,
# server-assigned ids/dates, connection-state stream positions, the
# commit_hash) PLUS the per-side login inputs — each side burns its own
# single-use code into the corpus, and the database directories are
# per-side mktemp paths.
normalize() {
  python3 -c "$(cat <<'PY'
import json
import re
import sys

COMMIT_HASH_RE = re.compile(
    r'("commit_hash","value":\{"@type":"optionValueString","value":")[^"]*"'
)
# Server-mutable option VALUES: the market rate drifts between the two
# live runs (observed 1623256 vs 1622970 within minutes).
MASKED_OPTION_VALUES = {"authorization_date", "utc_time_offset", "million_gram_to_usd_rate"}
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
        if type_name == "authenticationCodeInfo":
            # The server decides per request whether an SMS fallback is
            # offered (next_type present or absent) — client-side policy
            # is identical either way.
            obj.pop("next_type", None)
        for key in MASKED_KEYS:
            if key in obj:
                obj[key] = "<id>"
        return {k: v for k, v in ((k, mask(v)) for k, v in obj.items()) if v is not None}
    if isinstance(obj, list):
        return [v for v in (mask(v) for v in obj) if v is not None]
    return obj


def mask_request(obj):
    # Per-side login inputs: the single-use code (each side burns its
    # own), the optional password, and the per-side database paths.
    if not isinstance(obj, dict):
        return obj
    obj = dict(obj)
    type_name = obj.get("@type")
    if type_name == "setTdlibParameters":
        obj["database_directory"] = "<dir>"
        obj["files_directory"] = "<dir>"
    elif type_name == "checkAuthenticationCode":
        obj["code"] = "<code>"
    elif type_name == "checkAuthenticationPassword":
        obj["password"] = "<pw>"
    return obj


def dump(record):
    out = {}
    for kind in ("request", "update", "response"):
        if kind in record:
            out[kind] = json.dumps(record[kind], separators=(",", ":"), ensure_ascii=False)
    return json.dumps(out, separators=(",", ":"), ensure_ascii=False)


# Group by request window: every REAL request record starts a new group;
# the async records behind it belong to that request's window. Within a
# window the completion ORDER is scheduler/actor nondeterminism on both
# sides (the C++ defers the animation fan-out through actor hops, the
# port fires it before the option flush) — sorted comparison, so the
# window MEMBERSHIP is what the diff pins.
groups = []
current = None
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    line = COMMIT_HASH_RE.sub('\\1<git>"', line)
    record = json.loads(line)
    update = record.get("update")
    is_state = (
        isinstance(update, str) and '"@type":"updateConnectionState"' in update
    ) or (
        isinstance(update, dict) and update.get("@type") == "updateConnectionState"
    )
    if is_state:
        continue
    request = record.get("request")
    if isinstance(request, dict) and ("@wait" in request or "@idle" in request):
        continue  # harness bookkeeping markers
    if "request" in record:
        record["request"] = mask_request(record["request"])
        current = [dump(record)]
        groups.append(current)
        continue
    if current is None:
        continue
    masked = mask(record)
    if not masked:
        continue
    current.append(dump(masked))

for group in groups:
    head, tail = group[0], sorted(group[1:])
    print(head)
    for record in tail:
        print(record)
PY
)"
}

normalize <"$WORK/c.txt" >"$WORK/c.norm"
normalize <"$WORK/rs.txt" >"$WORK/rs.norm"

if ! cmp -s "$WORK/c.norm" "$WORK/rs.norm"; then
  echo "MISMATCH:" >&2
  diff "$WORK/c.norm" "$WORK/rs.norm" | head -80 >&2
  exit 1
fi
printf 'OK  %d records byte-identical modulo the S75 nondeterminism\n' \
  "$(wc -l <"$WORK/c.norm")"
echo "S75 user-path verification OK"
