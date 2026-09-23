#!/usr/bin/env bash
# S78 long-running fuzz session: cycles the fifteen targets forever, keeping
# per-target corpora (coverage-guided discovery accumulates across chunks)
# and logging every cycle. A genuine libFuzzer failure exit is a FINDING: the
# crashing input is preserved under fuzz/artifacts/ and fuzzing moves on.
#
# Launch (named tmux session, survives the launching shell):
#   tmux new-session -d -s tdrs-fuzz 'bash fuzz/run-week.sh'
#
# The plan's S78 gate is one week fuzz-clean per target; the weekly audit
# is `grep FINDING fuzz/fuzz-week.log` after the deadline.
#
# The 09-23 audit (fuzz/audit-2026-09-23.md) hardened the classification:
# rc=72 (libFuzzer "run interrupted" — SIGINT during a rebuild window) and a
# missing binary (the 09-19..09-21 window logged 35 phantom FINDINGs because
# the M.4.17 targets were added to TARGETS without ever building them) are
# NOT findings. Missing binaries SKIP before invoke; genuine libFuzzer
# failure exits (70 timeout/OOM, 77 deadly signal, panic-abort 134, ...)
# stay FINDINGs.
set -u

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CORPUS="$ROOT/fuzz/corpus"
ARTIFACTS="$ROOT/fuzz/artifacts"
LOG="$ROOT/fuzz/fuzz-week.log"
BIN="$ROOT/fuzz/target/x86_64-unknown-linux-gnu/release"
TARGETS=(td_api_json json_value binlog_event mtproto_packet tl_scheme_text transport_frame app_config simple_config user_log_event user_full secret_chat emoji_status message_content message_input updates_container)
CHUNK=${FUZZ_CHUNK:-1800} # seconds per target per cycle (30 min)
SEEDGEN="$ROOT/fuzz/seed-gen/target/release/gen_mtproto_seeds"

mkdir -p "$CORPUS" "$ARTIFACTS"
cd "$ROOT/fuzz" || exit 1

# mtproto_packet's corpus needs valid-format packets: the fixed-key v1
# encrypted envelope rejects essentially every random input (0 new units over
# 600M execs in the 09-16..09-21 week). The seed generator emits fresh
# server-direction (x=8) packets under the same dummy key the target uses;
# regenerate at week start so the corpus always opens past the format gate.
if [ -x "$SEEDGEN" ]; then
  (cd "$ROOT" && "$SEEDGEN" "$CORPUS/mtproto_packet") >> "$LOG" 2>&1 \
    || echo "[$(date -Is)] seed-gen FAILED rc=$?" >> "$LOG"
else
  echo "[$(date -Is)] seed-gen missing ($SEEDGEN) — build it before the week (fuzz/seed-gen/)" >> "$LOG"
fi

echo "== fuzz week started $(date -Is) on $(hostname), chunk ${CHUNK}s per target ==" >> "$LOG"
while true; do
  for t in "${TARGETS[@]}"; do
    mkdir -p "$CORPUS/$t"
    if [ ! -x "$BIN/$t" ]; then
      echo "[$(date -Is)] target $t: SKIP missing binary (cargo fuzz build first)" >> "$LOG"
      continue
    fi
    echo "[$(date -Is)] target $t: chunk ${CHUNK}s" >> "$LOG"
    "$BIN/$t" "$CORPUS/$t" \
      -max_total_time="$CHUNK" \
      -rss_limit_mb=4096 \
      -timeout=25 \
      -print_final_stats=1 >> "$LOG" 2>&1
    rc=$?
    if [ "$rc" -eq 0 ]; then
      echo "[$(date -Is)] target $t: rc=0" >> "$LOG"
    elif [ "$rc" -eq 72 ]; then
      echo "[$(date -Is)] target $t: rc=72 (run interrupted — not a FINDING)" >> "$LOG"
    elif [ "$rc" -eq 127 ]; then
      echo "[$(date -Is)] target $t: rc=127 (missing binary — not a FINDING)" >> "$LOG"
    else
      echo "[$(date -Is)] target $t: rc=$rc" >> "$LOG"
      echo "[$(date -Is)] FINDING in $t — crashing inputs preserved under fuzz/artifacts/" >> "$LOG"
      mv ./crash-* "$ARTIFACTS/" 2>/dev/null || true
      mv ./leak-* "$ARTIFACTS/" 2>/dev/null || true
      mv ./oom-* "$ARTIFACTS/" 2>/dev/null || true
      mv ./timeout-* "$ARTIFACTS/" 2>/dev/null || true
    fi
  done
done
