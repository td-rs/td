#!/usr/bin/env bash
# The M.10.22 flipped-fns live gate: both sides run LIVE against the test
# DC on the flipped_corpus_gen.sh corpus (the bot-token login + the
# deterministic-offline subset of the dispatcher flip wave) and the two
# streams diff modulo the S76 nondeterminism normalization. This is the
# S74 recording round for the flipped fns — the corpus deliberately
# excludes the query-tailed flips whose Rust promises park at the
# [blocked: net] seams while the C++ answers for real; the live
# send/receive session itself is the S76 gate (currently RED on the
# GetDifference net seam, closing with mnet-rb-mm + M.7.12-15 + S75).
#
# Usage: ./verify_flipped_live.sh [td-repo-root]
# Env: TD_BOT_TOKEN, TD_API_HASH (required); TD_RECORD_LINGER_MS (default
# 5000).
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
TD_ROOT=${1:-"$SCRIPT_DIR/../../../td"}
TD_BUILD="${TD_BUILD_DIR:-$TD_ROOT/TD_BUILD}"
RS_ROOT="$SCRIPT_DIR/../.."
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
LINGER="${TD_RECORD_LINGER_MS:-5000}"

: "${TD_BOT_TOKEN:?test-DC bot token missing (create one with the bot_maker tool)}"
: "${TD_API_HASH:?test-DC api_hash missing}"

CORPUS="$("$SCRIPT_DIR/flipped_corpus_gen.sh" "$WORK")"

echo "== building the C++ recorder against libtdjson =="
c++ -O1 -o "$WORK/record" "$SCRIPT_DIR/cpp/record.cpp" \
  -I"$TD_BUILD" -I"$TD_ROOT" -L"$TD_BUILD" -ltdjson

echo "== building the Rust replayer =="
env -u RUSTC_WRAPPER cargo build -q \
  --manifest-path "$RS_ROOT/conformance/Cargo.toml" --bin recorder-replayer
RUST_BIN="${CARGO_TARGET_DIR:-$RS_ROOT/conformance/target}/debug/recorder-replayer"

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
    "${driver[@]}" <"$CORPUS" >"$out" 2>"$WORK/$side.err"
}

echo "== recording the live C++ session =="
run_side cpp "$WORK/c.txt"

echo "== replaying through the Rust client =="
run_side rs "$WORK/rs.txt"

# The S76 normalization rides along verbatim: separate live sessions have
# different timestamps and the login emits session-dependent options; the
# bot token makes both logins real and repeatable. The `@wait` markers are
# harness bookkeeping. The commit_hash interim masking rides along from the
# offline gate.
normalize() {
  python3 -c "$(cat <<'PY'
import json
import re
import sys

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
