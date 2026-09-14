#!/usr/bin/env bash
# S61 verification: auth-data persistence cross-read. The C++ tool and the
# Rust mirror write the same standalone auth-data file (auth key, handshake
# state, salt, server-time correction, session id) with the ported
# `store`/`parse` methods — the files must be byte-identical, and each side
# must dump the other's file to the same canonical text.
#
# Usage: ./verify.sh [td-repo-root]   (default: ../../../td relative to here)
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
TD_ROOT=${1:-"$SCRIPT_DIR/../../../td"}
TD_BUILD=${TD_BUILD:-"/tmp/opencode/td-build"}
RS_ROOT="$SCRIPT_DIR/../.."
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

CXX="${CXX:-c++}"
TDUTILS_A="$TD_BUILD/tdutils/libtdutils.a"
if [ ! -f "$TDUTILS_A" ]; then
  echo "error: $TDUTILS_A not found — build tdlib once (cmake) and set TD_BUILD" >&2
  exit 1
fi

echo "== extracting the OpenSSL include dir from the C++ build =="
OPENSSL_INC=$(python3 - "$TD_BUILD/compile_commands.json" <<'EOF'
import json, sys
with open(sys.argv[1]) as f:
    cc = json.load(f)
for entry in cc:
    parts = entry["command"].split()
    for i, part in enumerate(parts):
        if part == "-isystem" and "openssl" in parts[i + 1]:
            print(parts[i + 1])
            sys.exit(0)
sys.exit("openssl -isystem path not found in compile_commands.json")
EOF
)
OPENSSL_VER=$(basename "$(dirname "$OPENSSL_INC")" | sed 's/.*-openssl-\([0-9.]*\)-dev$/\1/')
for candidate in /nix/store/*-openssl-"$OPENSSL_VER"/lib; do
  case "$candidate" in
    *-dev*) ;;
    *)
      if file -L "$candidate/libcrypto.so.3" 2>/dev/null | grep -q "x86-64"; then
        OPENSSL_LIB="$candidate"
        break
      fi
      ;;
  esac
done
for candidate in /nix/store/*-zlib-*/lib; do
  case "$candidate" in
    *-dev*|*-static*) ;;
    *)
      if file -L "$candidate/libz.so.1" 2>/dev/null | grep -q "x86-64"; then
        ZLIB_LIB="$candidate"
        break
      fi
      ;;
  esac
done

echo "== building the C++ auth-data tool =="
$CXX -std=c++17 -O1 -w \
  -I"$TD_ROOT" -I"$TD_ROOT/td/generate/auto" -I"$TD_ROOT/tdactor" -I"$TD_ROOT/tdutils" \
  -I"$TD_BUILD/tdutils" -isystem "$OPENSSL_INC" \
  -o "$WORK/auth_data_rw_cpp" "$SCRIPT_DIR/cpp/auth_data_rw.cpp" \
  "$TD_ROOT/td/mtproto/AuthData.cpp" \
  "$TD_ROOT/td/mtproto/DhHandshake.cpp" \
  "$TDUTILS_A" -L"$OPENSSL_LIB" -L"$ZLIB_LIB" -lcrypto -lssl -lz -lpthread

echo "== building the Rust mirror =="
env -u RUSTC_WRAPPER cargo build --manifest-path "$RS_ROOT/conformance/Cargo.toml" \
  --bin auth_data_rw
CONF_TARGET="${CARGO_TARGET_DIR:-$RS_ROOT/conformance/target}/debug"

"$WORK/auth_data_rw_cpp" --write "$WORK/cpp.authdata" >"$WORK/cpp-write.txt"
"$CONF_TARGET/auth_data_rw" --write "$WORK/rust.authdata" >"$WORK/rust-write.txt"
diff -u "$WORK/cpp-write.txt" "$WORK/rust-write.txt"
diff -u "$WORK/cpp.authdata" "$WORK/rust.authdata"

echo "== cross-reading =="
"$WORK/auth_data_rw_cpp" --read "$WORK/cpp.authdata" >"$WORK/cpp-reads-cpp.txt"
"$CONF_TARGET/auth_data_rw" --read "$WORK/cpp.authdata" >"$WORK/rust-reads-cpp.txt"
diff -u "$WORK/cpp-reads-cpp.txt" "$WORK/rust-reads-cpp.txt"
"$WORK/auth_data_rw_cpp" --read "$WORK/rust.authdata" >"$WORK/cpp-reads-rust.txt"
"$CONF_TARGET/auth_data_rw" --read "$WORK/rust.authdata" >"$WORK/rust-reads-rust.txt"
diff -u "$WORK/cpp-reads-rust.txt" "$WORK/rust-reads-rust.txt"

echo "S61 authdata oracle: ALL GREEN"
