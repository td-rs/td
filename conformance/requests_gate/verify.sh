#!/usr/bin/env bash
# M.11.6 — the Requests slice gate. Re-run after every Wave-1 flip (the
# RUST_REWRITE_PLAN_REQUESTS_M11.md flip protocol, step 7): the census
# fixture must cover the generated scheme with the pinned state counts,
# the unknown-function error path must keep its byte-exact JSON, and the
# close/destroy drain semantics must survive requests parked mid-flight.
#
# Usage: ./verify.sh   (from any cwd; needs cargo in PATH)
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
RS_ROOT="$SCRIPT_DIR/../.."
cd "$RS_ROOT"

echo "== building the tdclient test binary =="
env -u RUSTC_WRAPPER cargo test -q -p tdclient --lib --no-run

run_test() {
  local filter=$1
  echo "== $filter =="
  env -u RUSTC_WRAPPER cargo test -q -p tdclient --lib -- "$filter"
}

# 1. Census: fixture == generated scheme, states valid, pinned counts.
run_test census_fixture_covers_every_scheme_function

# 2. Unknown-function parity (the 400 "Unknown class" JSON error path).
run_test to_request_reports_unknown_functions

# 3. Close/destroy sequence under load: parked requests are aborted with
#    the C++ drain errors, not leaked or answered stale.
run_test close_aborts_parked_requests_and_loses_their_answers
run_test destroy_answers_parked_requests_unauthorized

echo "M.11 requests gate OK"
