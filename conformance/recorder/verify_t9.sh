#!/usr/bin/env bash
# The T.9 live transfer gate, in Rust AND in the C++ build. Two halves:
#
# 1. Upload: each side runs preliminaryUploadFile over a local corpus and
#    prints UPLOADED_SIZE= once every byte reached the server (the
#    partial-remote semantics — no persistent id until the file is sent
#    somewhere, FileManager.cpp:4994). Both sizes must equal the corpus
#    size.
# 2. Download: each side registers a server-owned URL file (getRemoteFile —
#    the externally generated `#url#` node, FileManager.cpp:442-448) and
#    feeds the curl snapshot through writeGeneratedFilePart /
#    finishFileGeneration, then prints PATH=/SIZE= once the local copy
#    completes. The script sha256s both copies and the curl snapshot —
#    all three values must match.
#
# Usage: ./verify_t9.sh [td-repo-root]
# Env: TD_BOT_TOKEN, TD_BOT_USERNAME, TD_API_HASH (fall back to the
# bot_maker --fetch path when missing; TD_PEER_STATE defaults to
# /tmp/s66_state.bin), TD_BUILD_DIR (default <td-repo-root>/TD_BUILD),
# TD_CORPUS_SIZE (default 262144), TD_CORPUS_URL (default
# https://telegram.org/img/t_logo.png).
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
TD_ROOT=${1:-"$SCRIPT_DIR/../../../td"}
TD_BUILD="${TD_BUILD_DIR:-$TD_ROOT/TD_BUILD}"
RS_ROOT="$SCRIPT_DIR/../.."
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
CORPUS_SIZE="${TD_CORPUS_SIZE:-262144}"
CORPUS_URL="${TD_CORPUS_URL:-https://telegram.org/img/t_logo.png}"

: "${TD_PEER_STATE:=/tmp/s66_state.bin}"

echo "== credentials =="
if [ -z "${TD_BOT_TOKEN:-}" ] || [ -z "${TD_BOT_USERNAME:-}" ] || [ -z "${TD_API_HASH:-}" ]; then
  echo "fetching the bot token through the persisted S66 phone session"
  env -u RUSTC_WRAPPER cargo build -q \
    --manifest-path "$RS_ROOT/conformance/Cargo.toml" --bin bot_maker
  PEER_SENDER="${TD_PEER_SENDER:-${CARGO_TARGET_DIR:-$RS_ROOT/conformance/target}/debug/bot_maker}"
  FETCH_OUT="$("$PEER_SENDER" "$TD_PEER_STATE" --fetch)"
  echo "bot_maker --fetch: $FETCH_OUT"
  TD_BOT_TOKEN=$(printf '%s\n' "$FETCH_OUT" | sed -n 's/^BOT_TOKEN=//p')
  TD_BOT_USERNAME=$(printf '%s\n' "$FETCH_OUT" | sed -n 's/^BOT_USERNAME=//p')
  : "${TD_BOT_TOKEN:?bot_maker --fetch returned no token}"
  : "${TD_BOT_USERNAME:?bot_maker --fetch returned no username}"
fi
# The api credentials ride in the phone-account state (the bot_maker MTA2
# layout: "MTA2" | dc u8 | wall_anchor f64 | phone u32-len str | api_id
# i32 LE | api_hash u32-len str) when not set — the same state the
# bot_maker fetch just used.
if [ -z "${TD_API_ID:-}" ] || [ -z "${TD_API_HASH:-}" ]; then
  STATE_CREDS=$(python3 -c '
import struct, sys
data = open(sys.argv[1], "rb").read()
pos = 4 + 1 + 8
def take_str():
    global pos
    length = struct.unpack_from("<I", data, pos)[0]
    pos += 4
    value = data[pos:pos + length].decode()
    pos += length
    return value
take_str()  # phone
api_id = struct.unpack_from("<i", data, pos)[0]
pos += 4
api_hash = take_str()
print(api_id)
print(api_hash)
' "$TD_PEER_STATE")
  TD_API_ID="${TD_API_ID:-$(printf '%s\n' "$STATE_CREDS" | sed -n 1p)}"
  TD_API_HASH="${TD_API_HASH:-$(printf '%s\n' "$STATE_CREDS" | sed -n 2p)}"
fi
: "${TD_API_HASH:?test-DC api_hash missing}"

echo "== building the drivers =="
echo "-- Rust transfer-gate --"
env -u RUSTC_WRAPPER cargo build -q \
  --manifest-path "$RS_ROOT/conformance/Cargo.toml" --bin transfer-gate
RS_BIN="${CARGO_TARGET_DIR:-$RS_ROOT/conformance/target}/debug/transfer-gate"
echo "-- C++ transfer-gate against libtdjson --"
c++ -O1 -o "$WORK/transfer_gate_cpp" "$SCRIPT_DIR/cpp/transfer_gate.cpp" \
  -I"$TD_BUILD" -I"$TD_ROOT" -L"$TD_BUILD" -ltdjson

echo "== generating the corpus =="
CORPUS="$WORK/corpus.bin"
python3 - "$CORPUS" "$CORPUS_SIZE" <<'PY'
import random, sys
random.seed(20260920)
with open(sys.argv[1], "wb") as f:
    size = int(sys.argv[2])
    f.write(random.randbytes(size))
PY
CORPUS_HASH=$(sha256sum "$CORPUS" | cut -d' ' -f1)
echo "corpus: $CORPUS_SIZE bytes, sha256 $CORPUS_HASH"

run_side() {
  local side=$1
  local out=$2
  local files_dir="$WORK/files_$side"
  local db_dir="$WORK/db_$side"
  mkdir -p "$files_dir" "$db_dir"
  local driver
  if [ "$side" = cpp ]; then
    driver=("$WORK/transfer_gate_cpp")
  else
    driver=("$RS_BIN")
  fi
  stdbuf -oL env LD_LIBRARY_PATH="$TD_BUILD" \
    TD_API_ID="$TD_API_ID" TD_API_HASH="$TD_API_HASH" \
    TD_BOT_TOKEN="$TD_BOT_TOKEN" TD_CORPUS_FILE="$CORPUS" \
    TD_CORPUS_URL="$CORPUS_URL" TD_SNAPSHOT_FILE="$SNAPSHOT" \
    TD_DB_DIR="$db_dir" TD_FILES_DIR="$files_dir/" \
    "${driver[@]}" >"$out" 2>"$WORK/$side.err"
}

echo "== fetching the download snapshot =="
SNAPSHOT="$WORK/snapshot.bin"
curl -fsS -o "$SNAPSHOT" "$CORPUS_URL"
SNAPSHOT_HASH=$(sha256sum "$SNAPSHOT" | cut -d' ' -f1)
echo "snapshot: $(stat -c %s "$SNAPSHOT") bytes, sha256 $SNAPSHOT_HASH"

echo "== running the C++ side =="
run_side cpp "$WORK/cpp.out"
echo "== running the Rust side =="
run_side rs "$WORK/rs.out"

cpp_uploaded=$(sed -n 's/^UPLOADED_SIZE=//p' "$WORK/cpp.out")
rs_uploaded=$(sed -n 's/^UPLOADED_SIZE=//p' "$WORK/rs.out")
: "${cpp_uploaded:?cpp side printed no UPLOADED_SIZE}"
: "${rs_uploaded:?rust side printed no UPLOADED_SIZE}"
if [ "$cpp_uploaded" != "$CORPUS_SIZE" ] || [ "$rs_uploaded" != "$CORPUS_SIZE" ]; then
  echo "UPLOAD MISMATCH:" >&2
  echo "  corpus size  $CORPUS_SIZE" >&2
  echo "  cpp uploaded $cpp_uploaded" >&2
  echo "  rust uploaded $rs_uploaded" >&2
  exit 1
fi
echo "upload OK: both sides pushed $CORPUS_SIZE bytes to the test DC"

cpp_path=$(sed -n 's/^PATH=//p' "$WORK/cpp.out")
cpp_size=$(sed -n 's/^SIZE=//p' "$WORK/cpp.out")
rs_path=$(sed -n 's/^PATH=//p' "$WORK/rs.out")
rs_size=$(sed -n 's/^SIZE=//p' "$WORK/rs.out")
: "${cpp_path:?cpp side printed no PATH}"
: "${rs_path:?rust side printed no PATH}"
echo "cpp copy: $cpp_path ($cpp_size bytes)"
echo "rs  copy: $rs_path ($rs_size bytes)"

CPP_HASH=$(sha256sum "$cpp_path" | cut -d' ' -f1)
RS_HASH=$(sha256sum "$rs_path" | cut -d' ' -f1)

if [ "$CPP_HASH" != "$SNAPSHOT_HASH" ] || [ "$RS_HASH" != "$SNAPSHOT_HASH" ]; then
  echo "MISMATCH:" >&2
  echo "  snapshot sha256 $SNAPSHOT_HASH" >&2
  echo "  cpp      sha256 $CPP_HASH" >&2
  echo "  rust     sha256 $RS_HASH" >&2
  exit 1
fi
echo "OK  download round-trip byte-identical: sha256 $CPP_HASH"
echo "T.9 live transfer verification OK"
