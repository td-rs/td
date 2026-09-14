#!/usr/bin/env bash
# S60 verification: the auth-key generation handshake against a mock DC.
# A deterministic Rust mock server speaks the server half of the protocol
# and validates every client message (unencrypted framing, RSA blob, DH
# math, nonces, SHA-1 wrapping). The C++ td client and the Rust port each
# run the full handshake against a fresh mock; the mock's per-message log
# must be identical for both, and both clients must report success.
#
# Usage: ./verify.sh [td-repo-root]   (default: ../../../td relative to here)
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
TD_ROOT=${1:-"$SCRIPT_DIR/../../../td"}
TD_BUILD=${TD_BUILD:-"/tmp/opencode/td-build"}
RS_ROOT="$SCRIPT_DIR/../.."
# Overridable so a failed run's client/mock logs stay inspectable.
WORK=${S60_WORK:-$(mktemp -d)}
mkdir -p "$WORK"
MOCK_PID=""
WORK_DONE=""
trap 'kill $MOCK_PID 2>/dev/null || true; [ -n "$WORK_DONE" ] && rm -rf "$WORK"' EXIT

CXX="${CXX:-c++}"
TDUTILS_A="$TD_BUILD/tdutils/libtdutils.a"
TDACTOR_A="$TD_BUILD/tdactor/libtdactor.a"
if [ ! -f "$TDUTILS_A" ] || [ ! -f "$TDACTOR_A" ]; then
  echo "error: td build libs not found — build tdlib once (cmake) and set TD_BUILD" >&2
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

echo "== building the C++ handshake client =="
$CXX -std=c++17 -O1 -w \
  -I"$TD_ROOT" -I"$TD_ROOT/td/generate/auto" -I"$TD_ROOT/tdactor" -I"$TD_ROOT/tdutils" \
  -I"$TD_ROOT/tdnet" -I"$TD_BUILD/tdutils" -isystem "$OPENSSL_INC" \
  -o "$WORK/handshake_client_cpp" "$SCRIPT_DIR/cpp/handshake_client.cpp" \
  "$TD_ROOT/td/mtproto/RawConnection.cpp" \
  "$TD_ROOT/td/mtproto/Handshake.cpp" \
  "$TD_ROOT/td/mtproto/HandshakeActor.cpp" \
  "$TD_ROOT/td/mtproto/DhHandshake.cpp" \
  "$TD_ROOT/td/mtproto/TcpTransport.cpp" \
  "$TD_ROOT/td/mtproto/IStreamTransport.cpp" \
  "$TD_ROOT/td/mtproto/TlsReaderByteFlow.cpp" \
  "$TD_ROOT/td/mtproto/Transport.cpp" \
  "$TD_ROOT/td/mtproto/KDF.cpp" \
  "$TD_ROOT/td/mtproto/ProxySecret.cpp" \
  "$TD_ROOT/td/mtproto/RSA.cpp" \
  "$TD_ROOT/td/mtproto/utils.cpp" \
  "$TD_ROOT/td/mtproto/ConnectionManager.cpp" \
  "$TD_ROOT/td/mtproto/HttpTransport.cpp" \
  "$TD_ROOT/td/generate/auto/td/mtproto/mtproto_api.cpp" \
  "$TD_ROOT/tdnet/td/net/HttpReader.cpp" \
  "$TD_ROOT/tdnet/td/net/HttpQuery.cpp" \
  "$TD_ROOT/tdnet/td/net/HttpHeaderCreator.cpp" \
  "$TD_ROOT/tdnet/td/net/HttpFile.cpp" \
  "$TD_ROOT/tdnet/td/net/HttpChunkedByteFlow.cpp" \
  "$TD_ROOT/tdnet/td/net/HttpContentLengthByteFlow.cpp" \
  "$TDACTOR_A" "$TDUTILS_A" -L"$OPENSSL_LIB" -L"$ZLIB_LIB" -lcrypto -lssl -lz -lpthread

echo "== building the Rust mock DC and client (release: 2048-bit RSA keygen is unusably slow in debug) =="
env -u RUSTC_WRAPPER cargo build --release --manifest-path "$RS_ROOT/conformance/Cargo.toml" \
  --bin mock_dc --bin handshake_client
CONF_TARGET="${CARGO_TARGET_DIR:-$RS_ROOT/conformance/target}/release"

run_client() {
  local name="$1" client="$2" mock_log="$3"
  local port=$((34000 + RANDOM % 1000))
  "$CONF_TARGET/mock_dc" "$port" "$WORK/$name-public.pem" "$WORK/$name-private.pem" >"$mock_log" 2>&1 &
  local mock_pid=$!
  MOCK_PID=$mock_pid
  # Wait until the mock is actually accepting — it logs "listening:ok" after
  # bind, which is the only reliable signal (the PEM files are written before
  # bind, and RSA keygen can take a while even in release).
  for _ in $(seq 1 600); do
    grep -q "listening:ok" "$mock_log" 2>/dev/null && break
    kill -0 "$mock_pid" 2>/dev/null || break
    sleep 0.1
  done
  timeout 60 "$client" "$port" "$WORK/$name-public.pem" >"$WORK/$name-client.log" 2>&1 || true
  # The mock exits after validating one handshake; give it 10 s and don't
  # hang forever if the client died before reaching it.
  for _ in $(seq 1 100); do
    kill -0 "$mock_pid" 2>/dev/null || break
    sleep 0.1
  done
  kill "$mock_pid" 2>/dev/null || true
  wait "$mock_pid" || true
}

run_client cpp "$WORK/handshake_client_cpp" "$WORK/cpp-mock.log"
run_client rust "$CONF_TARGET/handshake_client" "$WORK/rust-mock.log"

echo "== checking both clients report success =="
grep -q "handshake:ok" "$WORK/cpp-client.log"
grep -q "handshake:ok" "$WORK/rust-client.log"

echo "== diffing the mock's per-message validation log =="
diff -u "$WORK/cpp-mock.log" "$WORK/rust-mock.log"

echo "S60 handshake oracle: ALL GREEN"
WORK_DONE=1
