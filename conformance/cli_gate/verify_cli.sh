#!/usr/bin/env bash
# M.14.7 — the CLI slice gate.
#
# Verifies that:
#   1. The command census fixture covers every arm from the C++ cli.cpp exactly
#      (each PORTED arm is routed — zero "Unknown command" — and every
#      unported arm fires exactly one "Unknown command").
#   2. `send_request` without a live client returns 0 and logs the request.
#   3. The basic harness properties (echo, unknown, EOF-hang, partial-line drop,
#      quit-drops-tail) all hold via the scripted stdin gate.
#
# Usage:
#   ./conformance/cli_gate/verify_cli.sh        # from any cwd
#   TD_CPP=/path/to/td REGEN=1 ./...            # regenerate the census fixture
#
# Requirements: cargo in PATH; rustfmt/clippy are not re-run here (run them
# separately on the gate's source tree before merging).
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
RS_ROOT="$SCRIPT_DIR/../.."
cd "$RS_ROOT"

# Optionally regenerate the census fixture against the C++ tree.
if [[ "${REGEN:-0}" == "1" ]]; then
    : "${TD_CPP:?set TD_CPP to the path of td/telegram/cli.cpp}"
    echo "== regenerating cli_census.txt from $TD_CPP =="
    "$SCRIPT_DIR/extract_cli_census.sh" "$TD_CPP" \
        > crates/tdcli/testdata/cli_census.txt
    echo "   done — $(wc -l < crates/tdcli/testdata/cli_census.txt) arms"
fi

echo "== building the tdcli test binary =="
env -u RUSTC_WRAPPER cargo test -q -p tdcli --bin td_cli --no-run 2>&1

run_test() {
    local filter=$1
    echo "== $filter =="
    env -u RUSTC_WRAPPER cargo test -q -p tdcli --bin td_cli -- "$filter"
}

# 1. The census gate: every arm either routed or reporting Unknown exactly once.
run_test on_cmd_routes_the_cpp_census_arms

# 2. send_request without a client returns 0 and logs.
run_test send_without_a_client_reports_the_request

# 3. The scripted stdin harness fixtures.
echo "== integration tests (harness) =="
env -u RUSTC_WRAPPER cargo test -q -p tdcli --test harness 2>&1

echo ""
echo "GATE_GREEN — cli gate passed"
