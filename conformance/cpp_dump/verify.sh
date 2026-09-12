#!/usr/bin/env bash
# S04 verification: the Rust-extracted vectors must be byte-identical to what
# the C++ side decodes from the same embedded data.cpp blobs.
#
#  1. stages a byte-identical copy of <td>/test/data.cpp with the shim data.h
#     and compiles the dump oracle;
#  2. runs the Rust extractor (conformance crate) against the same td repo;
#  3. byte-compares every blob (cmp) and re-checks sha256sums.txt;
#  4. sanity-checks magic bytes (JPEG, gzip) on the decoded files.
#
# Usage: ./verify.sh [td-repo-root]   (default: ../../../td relative to here)
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
TD_ROOT=${1:-"$SCRIPT_DIR/../../../td"}
TD_DATA="$TD_ROOT/test/data.cpp"
VEC_DIR="$SCRIPT_DIR/../vectors"
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

[ -f "$TD_DATA" ] || { echo "not found: $TD_DATA (pass the td repo root)" >&2; exit 1; }

# Stage sources; the copy must stay byte-identical to the original.
cp "$TD_DATA" "$WORK/data.cpp"
cp "$SCRIPT_DIR/dump.cpp" "$SCRIPT_DIR/data.h" "$WORK/"
src_sha=$(sha256sum <"$TD_DATA" | cut -d' ' -f1)
copy_sha=$(sha256sum <"$WORK/data.cpp" | cut -d' ' -f1)
[ "$src_sha" = "$copy_sha" ] || { echo "staged data.cpp copy differs" >&2; exit 1; }

echo "== building and running the C++ dump oracle =="
g++ -std=c++17 -O2 -Wall -Wextra "$WORK/dump.cpp" -o "$WORK/dump"
"$WORK/dump" "$WORK"

echo "== running the Rust extractor =="
env -u RUSTC_WRAPPER cargo run -q --manifest-path "$SCRIPT_DIR/../Cargo.toml" -- "$TD_ROOT" "$VEC_DIR"

echo "== byte-comparing Rust vectors vs C++ oracle =="
for cpp_bin in "$WORK"/*.cpp.bin; do
  name=$(basename "$cpp_bin" .cpp.bin)
  cmp "$VEC_DIR/$name.bin" "$cpp_bin"
  echo "OK  $name"
done

echo "== verifying sha256sums.txt =="
(cd "$VEC_DIR" && sha256sum -c sha256sums.txt)

echo "== magic-byte sanity =="
file "$VEC_DIR"/*.bin
grep -q 'JPEG image data' <(file -b "$VEC_DIR/thumbnail.bin")
# td's gzdecode uses inflateInit2(MAX_WBITS + 32) and accepts both zlib- and
# gzip-wrapped deflate; both blobs here are zlib streams. Decompress exactly
# one layer: gzip_bomb is a nested bomb by design, so its inner layer must
# not be inflated.
python3 - "$VEC_DIR/gzip.bin" "$VEC_DIR/gzip_bomb.bin" <<'EOF'
import sys, zlib

for path in sys.argv[1:]:
    with open(path, "rb") as f:
        data = f.read()
    if data[0] != 0x78:
        sys.exit(f"{path}: not a zlib stream")
    zlib.decompress(data)  # raises on corrupt input
    print(f"OK  zlib stream verified: {path}")
EOF
# The sqlite_sample_db_* files are SQLCipher-encrypted, so they have no
# plaintext SQLite magic to check.

echo "S04 verification OK"
