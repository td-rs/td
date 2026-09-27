#!/usr/bin/env bash
# S82 probe build: nightly +atomics pthread wasm build of tdjson staticlib.
set -o pipefail
cd "${TD_WASM_TREE:-/tmp/tdrs-s82-rel}" || exit 1
export TMPDIR=/tmp RUSTC_WRAPPER=sccache
export CARGO_TARGET_DIR=${TD_WASM_TREE:-/tmp/tdrs-s82-rel}/target-pthread
export CC=emcc CXX=em++ AR=emar RANLIB=emranlib
export CFLAGS="-pthread -DSQLCIPHER_CRYPTO_LIBTOMCRYPT -I${LTC_HEADERS:-/tmp/ltc/src/headers}"
export CXXFLAGS="-pthread"
export RUSTFLAGS="-C target-feature=+atomics,+bulk-memory,+mutable-globals"
rustup run nightly cargo build -Z build-std=std,panic_abort --locked \
  --target wasm32-unknown-emscripten -p tdjson 2>&1 | tail -15
echo "BUILD_EXIT_${PIPESTATUS[0]}"
