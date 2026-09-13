#!/usr/bin/env bash
# S08 verification: the Rust tdtl .tlo serializer (`write_types` port) must
# produce bytes identical to td's own tl-parser (the C original, compiled
# unmodified) for all five schemes. This covers the combinator argument
# trees (fields, flags, optional/conditional args, arrays, nat vars), which
# the S07 text dump did not.
#
# Usage: ./verify_tlo.sh [td-repo-root]   (default: ../../../td relative to here)
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
cc -O2 -w -o "$WORK/dump_tlo" "$SCRIPT_DIR/dump_tlo.c" \
  "$TL_PARSER_DIR/tl-parser.c" "$TL_PARSER_DIR/crc32.c" -I"$TL_PARSER_DIR"

echo "== building the Rust tl-tlo =="
env -u RUSTC_WRAPPER cargo build -q --release --manifest-path "$RS_ROOT/Cargo.toml" -p tdtl
RUST_BIN="$RS_ROOT/target/release/tl-tlo"

for scheme in "$SCHEME_DIR"/*.tl; do
  name=$(basename "$scheme" .tl)
  "$WORK/dump_tlo" "$scheme" >"$WORK/$name.c.tlo"
  "$RUST_BIN" "$scheme" >"$WORK/$name.rs.tlo"
  if ! cmp -s "$WORK/$name.c.tlo" "$WORK/$name.rs.tlo"; then
    echo "MISMATCH in $name:" >&2
    wc -c "$WORK/$name.c.tlo" "$WORK/$name.rs.tlo" >&2
    cmp "$WORK/$name.c.tlo" "$WORK/$name.rs.tlo" >&2 || true
    exit 1
  fi
  printf 'OK  %-14s %8d bytes  %s\n' "$name" \
    "$(wc -c <"$WORK/$name.c.tlo")" "$(sha256sum <"$WORK/$name.c.tlo" | cut -c1-16)"
done

echo "S08 verification OK"
