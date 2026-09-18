#!/usr/bin/env bash
# S78 long-running fuzz session: cycles the ten targets forever, keeping
# per-target corpora (coverage-guided discovery accumulates across chunks)
# and logging every cycle. A non-zero libFuzzer exit is a FINDING: the
# crashing input is preserved under fuzz/artifacts/ and fuzzing moves on.
#
# Launch (named tmux session, survives the launching shell):
#   tmux new-session -d -s tdrs-fuzz 'bash fuzz/run-week.sh'
#
# The plan's S78 gate is one week fuzz-clean per target; the weekly audit
# is `grep FINDING fuzz/fuzz-week.log` after the deadline.
set -u

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CORPUS="$ROOT/fuzz/corpus"
ARTIFACTS="$ROOT/fuzz/artifacts"
LOG="$ROOT/fuzz/fuzz-week.log"
BIN="$ROOT/fuzz/target/x86_64-unknown-linux-gnu/release"
TARGETS=(td_api_json json_value binlog_event mtproto_packet tl_scheme_text transport_frame app_config simple_config user_log_event user_full secret_chat emoji_status updates_container)
CHUNK=${FUZZ_CHUNK:-1800} # seconds per target per cycle (30 min)

mkdir -p "$CORPUS" "$ARTIFACTS"
cd "$ROOT/fuzz" || exit 1

echo "== fuzz week started $(date -Is), chunk ${CHUNK}s per target ==" >> "$LOG"
while true; do
  for t in "${TARGETS[@]}"; do
    mkdir -p "$CORPUS/$t"
    echo "[$(date -Is)] target $t: chunk ${CHUNK}s" >> "$LOG"
    "$BIN/$t" "$CORPUS/$t" \
      -max_total_time="$CHUNK" \
      -rss_limit_mb=4096 \
      -timeout=25 \
      -print_final_stats=1 >> "$LOG" 2>&1
    rc=$?
    echo "[$(date -Is)] target $t: rc=$rc" >> "$LOG"
    if [ "$rc" -ne 0 ]; then
      echo "[$(date -Is)] FINDING in $t — crashing inputs preserved under fuzz/artifacts/" >> "$LOG"
      mv ./crash-* "$ARTIFACTS/" 2>/dev/null || true
      mv ./leak-* "$ARTIFACTS/" 2>/dev/null || true
      mv ./oom-* "$ARTIFACTS/" 2>/dev/null || true
      mv ./timeout-* "$ARTIFACTS/" 2>/dev/null || true
    fi
  done
done
