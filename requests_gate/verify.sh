#!/usr/bin/env bash
# The M.10.22 exhaustiveness audits — both giant switches:
#   1. the td_api request dispatcher vs the census fixture;
#   2. the M.7 fan-out census (the MessagesManager on_update overloads).
# TD_CPP overrides the C++ tree for the fan-out census (default: ../td).
set -uo pipefail
cd "$(dirname "$0")/.."
fail=0
python3 requests_gate/audit_dispatcher.py || fail=1
TD_CPP="${TD_CPP:-../td}" python3 requests_gate/audit_m7_fanout.py || fail=1
if [ "$fail" -ne 0 ]; then
  echo "AUDITS_RED"
  exit 1
fi
echo "AUDITS_GREEN"
