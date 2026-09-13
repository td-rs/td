#!/usr/bin/env bash
# S37 verification: the Rust actor runtime must stay within 2x of the C++
# one on every bench_actor case — the plan's "actor runtime at <=2x perf"
# milestone. Both binaries are the verbatim port pair (benchmark/
# bench_actor.cpp vs bench/src/bench_actor.rs); cases are paired by their
# report-line description and compared on the average ops/sec.
#
# Usage: ./actor-gate.sh [td-repo-root]   (default: ../../td relative to here)
#
# Env:
#   TD_BUILD   existing C++ cmake build dir reused when present (the gate
#              builds `bench_actor` incrementally there); a fresh Release
#              build dir is configured otherwise.
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
TD_ROOT=${1:-"$SCRIPT_DIR/../../td"}
RS_ROOT="$SCRIPT_DIR/.."
TD_BUILD=${TD_BUILD:-}
RATIO_LIMIT=2.0
if [ -z "$TD_BUILD" ]; then
  # Reuse an existing configured build dir when one is present (the gate is
  # a perf comparison; configure-from-scratch is the slow fallback).
  for candidate in "$TD_ROOT"/build*/CMakeCache.txt; do
    if [ -f "$candidate" ]; then
      TD_BUILD=$(dirname "$candidate")
      break
    fi
  done
fi
if [ -z "$TD_BUILD" ]; then
  TD_BUILD=$(mktemp -d /tmp/td-bench-build.XXXXXX)
  cmake -S "$TD_ROOT" -B "$TD_BUILD" -DCMAKE_BUILD_TYPE=Release >/dev/null
fi
cmake --build "$TD_BUILD" --target bench_actor >/dev/null
CPP_BIN="$TD_BUILD/benchmark/bench_actor"

echo "== building the Rust bench_actor =="
(cd "$RS_ROOT/bench" && env -u RUSTC_WRAPPER cargo build --release --quiet)
echo "== running both (3 passes each, best pass wins per case) =="
CPP_OUTS=()
RS_OUTS=()
for pass in 1 2 3; do
  cpp_out=$(mktemp)
  rs_out=$(mktemp)
  "$CPP_BIN" 2>"$cpp_out"
  "$RS_ROOT/bench/target/release/bench_actor" 2>"$rs_out"
  CPP_OUTS+=("$cpp_out")
  RS_OUTS+=("$rs_out")
done
trap 'rm -f "${CPP_OUTS[@]}" "${RS_OUTS[@]}"' EXIT

extract() {
  # "Bench [desc]: avg[min-max] ops/sec" -> "desc\tavg", one line per case.
  sed -n 's/.*Bench \[\(.*\)\]: \([0-9.]*\)\[.*/\1\t\2/p' "$@"
}

echo
printf '%-42s %14s %14s %9s\n' "case" "C++ ops/sec" "Rust ops/sec" "slowdown"
fail=0
cpp_cases=$(extract "${CPP_OUTS[@]}" | sort -u -t$'\t' -k1,1)
while IFS=$'\t' read -r desc _; do
  # Best (max) across passes per side: background load only ever slows a
  # run down, so the maximum approximates the quiet-machine number.
  cpp_ops=$(extract "${CPP_OUTS[@]}" | grep -F "$desc"$'\t' | cut -f2 | sort -g | tail -1)
  rs_ops=$(extract "${RS_OUTS[@]}" | grep -F "$desc"$'\t' | cut -f2 | sort -g | tail -1)
  if [ -z "$cpp_ops" ] || [ -z "$rs_ops" ]; then
    printf '%-42s %14s %14s %9s\n' "$desc" "${cpp_ops:-MISSING}" "${rs_ops:-MISSING}" "-"
    fail=1
    continue
  fi
  slowdown=$(awk -v c="$cpp_ops" -v r="$rs_ops" 'BEGIN { printf "%.2f", c / r }')
  verdict=""
  ok=$(awk -v c="$cpp_ops" -v r="$rs_ops" -v lim="$RATIO_LIMIT" 'BEGIN { print (r >= c / lim) ? 1 : 0 }')
  if [ "$ok" != 1 ]; then
    verdict="  FAIL"
    fail=1
  fi
  printf '%-42s %14s %14s %8sx%s\n' "$desc" "$cpp_ops" "$rs_ops" "$slowdown" "$verdict"
done <<CASES
$cpp_cases
CASES

echo
if [ "$fail" != 0 ]; then
  echo "S37 gate FAILED: a case is slower than ${RATIO_LIMIT}x C++ or a case is missing"
  exit 1
fi
echo "S37 gate OK: every case within ${RATIO_LIMIT}x of C++"
