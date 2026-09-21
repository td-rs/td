#!/usr/bin/env bash
# M.14.2 — the CLI command census extractor.
#
# Walks the C++ `CliClient::on_cmd` (td/telegram/cli.cpp) and emits one
# `<op> <block>` line per command arm, in tree order: <block> is the index
# (0-based) of the sequential if/else-if chain the arm belongs to — the same
# 19 chains the Rust dispatcher mirrors as op_block_*. The chain boundary is
# the `else op_not_found_count++;` line (the chain tail, cli.cpp:3099+).
#
# Regenerate (from the td checkout, against the tree we build against):
#   conformance/cli_gate/extract_cli_census.sh path/to/cli.cpp \
#       > crates/tdcli/testdata/cli_census.txt
set -euo pipefail
: "${1:?usage: extract_cli_census.sh <path-to-cli.cpp>}"
CLI="$1"

awk '
  /^  void on_cmd\(string cmd\) \{/ { in_on_cmd = 1; chain = 0 }
  # Only the chain conditions (`    if (op == ...` / `    } else if (op == ...`)
  # count as arms — `op ==` comparisons inside arm bodies (e.g. the registerUser
  # third argument) are not commands.
  in_on_cmd && /^    (\} else )?if \(op == "/ {
    line = $0
    while (match(line, /op == "[^"]*"/)) {
      op = substr(line, RSTART + 7, RLENGTH - 8)
      print op, chain
      line = substr(line, RSTART + RLENGTH)
    }
  }
  # The chain tail (`} else { op_not_found_count++; }`) ends one of the 19
  # sequential if/else-if chains.
  in_on_cmd && /op_not_found_count\+\+;/ { chain++ }
  in_on_cmd && /^  \}$/ { exit }
' "$CLI"
