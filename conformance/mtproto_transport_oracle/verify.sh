#!/usr/bin/env bash
# S54 verification: differential transport-framing gate. td's own C++
# IntermediateTransport and the Rust port run the same deterministic
# abridged-framing matrix (write frames, read frames, quick-ack, partial
# reads) and read each other's random-padded frames; all outputs must be
# byte-identical.
#
# Usage: ./verify.sh [td-repo-root]   (default: ../../../td relative to here)
# The full C++ build is reused for libtdutils.a when present
# (TD_BUILD env var overrides the path).
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
try:
    with open(sys.argv[1]) as f:
        cc = json.load(f)
except FileNotFoundError:
    sys.exit("compile_commands.json not found")
for entry in cc:
    for part in entry["command"].split():
        if part.startswith("-isystem") and "openssl" in part:
            print(part.split("=", 1)[1] if "=" in part else "")
            sys.exit(0)
    for i, part in enumerate(entry["command"].split()):
        if part == "-isystem" and "openssl" in entry["command"].split()[i + 1]:
            print(entry["command"].split()[i + 1])
            sys.exit(0)
sys.exit("openssl -isystem path not found in compile_commands.json")
EOF
)
# The matching OpenSSL `out` (libcrypto.so/libssl.so) is a sibling store
# path of the `-dev` include dir (nix multi-output derivations get distinct
# hashes, so match by version). Loops instead of `ls | head` — a SIGPIPE in
# a pipeline kills the script under `set -euo pipefail`.
OPENSSL_VER=$(basename "$(dirname "$OPENSSL_INC")" | sed 's/.*-openssl-\([0-9.]*\)-dev$/\1/')
for candidate in /nix/store/*-openssl-"$OPENSSL_VER"/lib; do
  case "$candidate" in
    *-dev*) ;;
    *)
      # Several multi-platform builds coexist in the store; take an x86-64 one.
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

echo "== building the C++ transport oracle against td sources + libtdutils.a =="
$CXX -std=c++17 -O1 -w \
  -I"$TD_ROOT" -I"$TD_ROOT/td/generate/auto" -I"$TD_ROOT/tdactor" -I"$TD_ROOT/tdutils" \
  -I"$TD_ROOT/tdnet" -I"$TD_BUILD/tdutils" -isystem "$OPENSSL_INC" \
  -o "$WORK/dump_transport_cpp" "$SCRIPT_DIR/cpp/dump_transport.cpp" \
  "$TD_ROOT/td/mtproto/IStreamTransport.cpp" \
  "$TD_ROOT/td/mtproto/TcpTransport.cpp" \
  "$TD_ROOT/td/mtproto/TlsReaderByteFlow.cpp" \
  "$TD_ROOT/td/mtproto/HttpTransport.cpp" \
  "$TD_ROOT/td/mtproto/Transport.cpp" \
  "$TD_ROOT/td/mtproto/KDF.cpp" \
  "$TD_ROOT/td/mtproto/DhHandshake.cpp" \
  "$TD_ROOT/td/mtproto/ProxySecret.cpp" \
  "$TD_ROOT/tdnet/td/net/HttpReader.cpp" \
  "$TD_ROOT/tdnet/td/net/HttpQuery.cpp" \
  "$TD_ROOT/tdnet/td/net/HttpHeaderCreator.cpp" \
  "$TD_ROOT/tdnet/td/net/HttpFile.cpp" \
  "$TD_ROOT/tdnet/td/net/HttpChunkedByteFlow.cpp" \
  "$TD_ROOT/tdnet/td/net/HttpContentLengthByteFlow.cpp" \
  "$TDUTILS_A" -L"$OPENSSL_LIB" -L"$ZLIB_LIB" -lcrypto -lssl -lz -lpthread

echo "== building the Rust mirror =="
CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$RS_ROOT/target}" \
  env -u RUSTC_WRAPPER cargo build -p tdmtproto --bin dump_transport 2>/dev/null \
  || env -u RUSTC_WRAPPER cargo build --manifest-path "$RS_ROOT/Cargo.toml" \
     -p tdmtproto --bin dump_transport
DUMP_RUST="$CARGO_TARGET_DIR/debug/dump_transport"

# The deterministic matrix: payloads (empty, 1, 4, 16, 60 bytes) x quick_ack
# x {write, read}, plus the interesting read-only cases.
run_matrix() {
  local dump="$1" out="$2"
  : >"$out"
  local payload
  # Payloads must be 4-byte aligned — the MTProto layer guarantees it, and
  # the C++ CHECKs `size % 4 == 0` in write_prepare_inplace.
  for payload in "" "41424344" "30313233343536373839616263646566" "$(printf '61%.0s' $(seq 1 60))"; do
    for qa in 0 1; do
      "$dump" --abridged-write "$payload" "$qa" >>"$out"
      local frame
      frame=$("$dump" --abridged-write "$payload" "$qa" | cut -c3-)
      "$dump" --abridged-read "$frame" >>"$out"
    done
  done
  # quick-ack-only frame, a partial frame, and an empty stream
  "$dump" --abridged-read "00000080" >>"$out"
  "$dump" --abridged-read "1400000061626364" >>"$out"
  "$dump" --abridged-read "" >>"$out"
}

run_matrix "$WORK/dump_transport_cpp" "$WORK/cpp.txt"
run_matrix "$DUMP_RUST" "$WORK/rust.txt"

echo "== diffing the deterministic matrix =="
diff -u "$WORK/cpp.txt" "$WORK/rust.txt"

echo "== cross-reading padded frames =="
"$WORK/dump_transport_cpp" --padded-write-file "$WORK/cpp-padded.bin" 100
"$WORK/dump_transport_cpp" --read-file "$WORK/cpp-padded.bin" 1 >"$WORK/cpp-reads-cpp.txt"
"$DUMP_RUST" --read-file "$WORK/cpp-padded.bin" 1 >"$WORK/rust-reads-cpp.txt"
diff -u "$WORK/cpp-reads-cpp.txt" "$WORK/rust-reads-cpp.txt"

"$DUMP_RUST" --padded-write-file "$WORK/rust-padded.bin" 100
"$DUMP_RUST" --read-file "$WORK/rust-padded.bin" 1 >"$WORK/rust-reads-rust.txt"
"$WORK/dump_transport_cpp" --read-file "$WORK/rust-padded.bin" 1 >"$WORK/cpp-reads-rust.txt"
diff -u "$WORK/rust-reads-rust.txt" "$WORK/cpp-reads-rust.txt"

echo "== S55 obfuscated transport interop =="
# Classic 16-byte secret, dd-prefixed (random padding), ee-prefixed
# (TLS emulation). Each side generates streams, the other side decrypts
# them — the reads of one stream must agree byte-for-byte.
SECRETS=(
  "0102030405060708090a0b0c0d0e0f10"
  "dd0102030405060708090a0b0c0d0e0f10"
  "ee0102030405060708090a0b0c0d0e0f107777772e676f6f676c652e636f6d"
)
for secret in "${SECRETS[@]}"; do
  "$WORK/dump_transport_cpp" --obfuscated-write-file "$WORK/cpp-obf.bin" 40 "$secret"
  "$WORK/dump_transport_cpp" --obfuscated-read-file "$WORK/cpp-obf.bin" "$secret" >"$WORK/cpp-obf.txt"
  "$DUMP_RUST" --obfuscated-read-file "$WORK/cpp-obf.bin" "$secret" >"$WORK/rust-reads-cpp-obf.txt"
  diff -u "$WORK/cpp-obf.txt" "$WORK/rust-reads-cpp-obf.txt"

  "$DUMP_RUST" --obfuscated-write-file "$WORK/rust-obf.bin" 40 "$secret"
  "$DUMP_RUST" --obfuscated-read-file "$WORK/rust-obf.bin" "$secret" >"$WORK/rust-obf.txt"
  "$WORK/dump_transport_cpp" --obfuscated-read-file "$WORK/rust-obf.bin" "$secret" >"$WORK/cpp-reads-rust-obf.txt"
  diff -u "$WORK/rust-obf.txt" "$WORK/cpp-reads-rust-obf.txt"
done

echo "== S56 HTTP transport vectors =="
# Empty secret, plain host and host|authorization; the POST frames must be
# byte-identical.
for secret in "" "686f73742e6578616d706c65" "686f73742e6578616d706c657c757365723a70617373"; do
  for payload in "" "41" "41424344"; do
    "$WORK/dump_transport_cpp" --http-write "$payload" "$secret" >>"$WORK/cpp-http.txt"
    "$DUMP_RUST" --http-write "$payload" "$secret" >>"$WORK/rust-http.txt"
  done
done
diff -u "$WORK/cpp-http.txt" "$WORK/rust-http.txt"
# The read direction: fixed content-length and chunked responses.
RESP_CONTENT_LENGTH="485454502f312e3120323030204f4b0d0a436f6e74656e742d4c656e6774683a20340d0a0d0a41424344"
RESP_CHUNKED="485454502f312e3120323030204f4b0d0a5472616e736665722d456e636f64696e673a206368756e6b65640d0a0d0a343b616263640d0a303b0d0a"
for resp in "$RESP_CONTENT_LENGTH" "$RESP_CHUNKED"; do
  "$WORK/dump_transport_cpp" --http-read "$resp" "" >>"$WORK/cpp-http-read.txt"
  "$DUMP_RUST" --http-read "$resp" "" >>"$WORK/rust-http-read.txt"
done
diff -u "$WORK/cpp-http-read.txt" "$WORK/rust-http-read.txt"

echo "== S57 packet crypto vectors =="
# A valid body: msg_id=1, seq_no=0, length=4, payload aabb (the length field
# must agree with the actual payload).
# v2 message keys cover the random padding, so v2 writes are never
# deterministic — the write diff covers v1 prefixes only; v2 is verified
# through the read direction and the random-pad cross-reads below.
BODY="0100000000000000000000000400000061616262"
for body in "" "aabbccdd" "$BODY"; do
  # v1 pads with random bytes; only whole ciphertext blocks before the
  # pad block are deterministic (auth_key_id + msg_key + encrypted
  # salt/session + whole 16-byte blocks of the body).
  prefix_len=$((2 * (40 + ((${#body} / 2) & ~15))))
  "$WORK/dump_transport_cpp" --crypto-write "$body" 1 | cut -c1-"$prefix_len" >>"$WORK/cpp-crypto.txt"
  "$DUMP_RUST" --crypto-write "$body" 1 | cut -c1-"$prefix_len" >>"$WORK/rust-crypto.txt"
done
diff -u "$WORK/cpp-crypto.txt" "$WORK/rust-crypto.txt"
for version in 1 2; do
  frame_cpp=$("$WORK/dump_transport_cpp" --crypto-write "$BODY" "$version" | cut -c5-)
  "$WORK/dump_transport_cpp" --crypto-read-server "$frame_cpp" "$version" >>"$WORK/cpp-crypto-read.txt"
  "$DUMP_RUST" --crypto-read-server "$frame_cpp" "$version" >>"$WORK/rust-crypto-read.txt"
  frame_rust=$("$DUMP_RUST" --crypto-write "$BODY" "$version" | cut -c5-)
  "$WORK/dump_transport_cpp" --crypto-read-server "$frame_rust" "$version" >>"$WORK/cpp-crypto-read.txt"
  "$DUMP_RUST" --crypto-read-server "$frame_rust" "$version" >>"$WORK/rust-crypto-read.txt"
done
diff -u "$WORK/cpp-crypto-read.txt" "$WORK/rust-crypto-read.txt"
# Random padding (v2 only): each side generates, both sides decrypt.
for side in cpp rust; do
  dump="$WORK/dump_transport_cpp"; [ "$side" = rust ] && dump="$DUMP_RUST"
  frame=$("$dump" --crypto-write "$BODY" 2 1 | cut -c5-)
  "$WORK/dump_transport_cpp" --crypto-read-server "$frame" 2 >>"$WORK/cpp-crypto-rand.txt"
  "$DUMP_RUST" --crypto-read-server "$frame" 2 >>"$WORK/rust-crypto-rand.txt"
done
diff -u "$WORK/cpp-crypto-rand.txt" "$WORK/rust-crypto-rand.txt"

echo "S54+S55+S56+S57 transport oracle: ALL GREEN"
