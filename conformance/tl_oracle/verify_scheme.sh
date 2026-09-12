#!/usr/bin/env bash
# S07 verification: the Rust tdtl parser (parse tree + scheme program, with
# constructor ids from CRC32 of the canonical combinator string) must produce
# a scheme dump identical to td's own tl-parser (the C original, compiled
# unmodified) for all five schemes. This subsumes the type/function/ctor
# count check (td_api 750 types / 1022 functions, telegram_api 402 / 1333).
#
# Usage: ./verify_scheme.sh [td-repo-root]   (default: ../../../td relative to here)
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
TD_ROOT=${1:-"$SCRIPT_DIR/../../../td"}
TL_PARSER_DIR="$TD_ROOT/td/generate/tl-parser"
SCHEME_DIR="$SCRIPT_DIR/../../crates/tl-codegen/scheme"
RS_ROOT="$SCRIPT_DIR/../.."
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

[ -f "$TL_PARSER_DIR/tl-parser.c" ] || {
  echo "not found: $TL_PARSER_DIR/tl-parser.c (pass the td repo root)" >&2
  exit 1
}

echo "== building the C oracle from td's unmodified tl-parser =="
# -w: tl-parser.c is old C; we only care about its behavior, not warnings.
cc -O2 -w -o "$WORK/dump_scheme" "$SCRIPT_DIR/dump_scheme.c" \
  "$TL_PARSER_DIR/tl-parser.c" "$TL_PARSER_DIR/crc32.c" -I"$TL_PARSER_DIR"

echo "== building the Rust tl-scheme-dump =="
env -u RUSTC_WRAPPER cargo build -q --release --manifest-path "$RS_ROOT/Cargo.toml" -p tdtl
RUST_BIN="$RS_ROOT/target/release/tl-scheme-dump"

for scheme in "$SCHEME_DIR"/*.tl; do
  name=$(basename "$scheme" .tl)
  "$WORK/dump_scheme" "$scheme" >"$WORK/$name.c.txt"
  "$RUST_BIN" "$scheme" >"$WORK/$name.rs.txt"
  if ! cmp -s "$WORK/$name.c.txt" "$WORK/$name.rs.txt"; then
    echo "MISMATCH in $name:" >&2
    diff "$WORK/$name.c.txt" "$WORK/$name.rs.txt" | head -30 >&2
    exit 1
  fi
  head -1 "$WORK/$name.c.txt" | sed "s/^/OK  $name  /"
done

echo "S07 verification OK"
