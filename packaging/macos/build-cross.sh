#!/usr/bin/env bash
# S83 packaging (macOS) — the LINUX cross-build: produce the real macOS
# tdjson dylib from this workstation, no Apple SDK (zig's clean-room
# libSystem stubs compile the C/C++, raw ld64.lld links with the
# export list). Found 2026-09-28; this exact recipe shipped the
# macos-x64 artifact.
#
# The three walls this script exists for (each cost a debugging round):
# 1. zig cc DROPS -exported_symbols_list (allowlisted linker args), so a
#    zig-cc-linked dylib exports ~80k symbols — the final link must run
#    raw ld64.lld with the rustc-generated export list translated over.
# 2. rustc emits the export list as TWO adjacent -Wl args; ld64.lld wants
#    `-exported_symbols_list <path>` (and the path arrives -Wl,-prefixed).
# 3. zig ships only libSystem.tbd — `-liconv` (rustc's default darwin
#    link arg) needs a hand-written libiconv.tbd describing the real
#    system library (link- AND runtime-correct).
# Plus: sqlcipher wants Apple's os/log.h (define SQLCIPHER_OMIT_LOG_DEVICE
# — the supported escape hatch), cc-rs passes --target=x86_64-apple-macosx
# (zig rejects 'macosx' — the wrappers drop incoming target flags), and
# host ranlib mangles zig archives (RANLIB must be zig's).
#
# Usage: build-cross.sh <x86_64|aarch64> <out-dist-dir>
# Env: TD_CPP (the C++ tdlib checkout carrying build-d1 headers; default
# ~/Projects/td), TOOLCHAIN (rustup toolchain, default 1.95.0).
# Needs: nix-shell -p rustup zig lld llvmPackages.libllvm (the script
# re-executes itself inside that shell if missing).
set -euo pipefail

ARCH="${1:?usage: build-cross.sh <x86_64|aarch64> <out-dist-dir>}"
DIST="${2:?usage: build-cross.sh <x86_64|aarch64> <out-dist-dir>}"
VER=1.8.68
TD_CPP=${TD_CPP:-$HOME/Projects/td}
TOOLCHAIN=${TOOLCHAIN:-1.95.0}
RS_ROOT=$(cd "$(dirname "$0")/../.." && pwd)
TRIPLE=${ARCH}-apple-darwin
# zig's triple spelling (note: -none suffix, and macos NOT macosx)
ZTRIPLE=$([ "$ARCH" = x86_64 ] && echo x86_64-macos-none || echo aarch64-macos-none)
TBDTARGET=$([ "$ARCH" = x86_64 ] && echo x86_64-macos || echo arm64-macos)
TENV=$(echo "$TRIPLE" | tr - _ )   # x86_64_apple_darwin / aarch64_apple_darwin

# Re-exec inside the tool nix-shell when bare (zig/lld are nix-provided).
if ! command -v zig >/dev/null || ! command -v ld64.lld >/dev/null; then
  exec nix-shell -p rustup zig lld llvmPackages.libllvm --run "$0 $*"
fi

WORK=$(mktemp -d /tmp/tdrs-maccross-XXXXXX)
trap 'rm -rf "$WORK"' EXIT
ZIGLIB=$(zig env | sed -n 's/^ *\.lib_dir = *"\([^"]*\)".*/\1/p')/libc/darwin
[ -f "$ZIGLIB/libSystem.tbd" ] || { echo "libSystem.tbd not found at $ZIGLIB (zig layout changed?)"; exit 1; }

# --- 1. the tool wrappers (CC/CXX/AR/RANLIB + the ld64.lld linker) -------
cat >"$WORK/cc.sh" <<EOF
#!/usr/bin/env bash
# zig cc for $TRIPLE: drops incoming clang-style target flags (cc-rs passes
# --target=$TRIPLE-macosx; zig rejects 'macosx' and has its own -target).
args=(); skip=0
for a in "\$@"; do
  [ "\$skip" = 1 ] && { skip=0; continue; }
  case "\$a" in
    --target=*|-target) continue ;;
    -target) skip=1; continue ;;
  esac
  args+=("\$a")
done
exec zig cc -target $ZTRIPLE "\${args[@]}"
EOF
sed 's/zig cc/zig c++/' "$WORK/cc.sh" >"$WORK/cxx.sh"
printf '#!/usr/bin/env bash\nexec zig ar "$@"\n' >"$WORK/ar.sh"
printf '#!/usr/bin/env bash\nexec zig ranlib "$@"\n' >"$WORK/ranlib.sh"
cat >"$WORK/linker.sh" <<EOF
#!/usr/bin/env bash
# ld64.lld driver: rustc's clang-style link args translated live (the
# temp export list + codegen objects only exist during the rustc call).
set -e
A=("\$@")
out=("-L$ZIGLIB" "-L$WORK/lib" "-install_name" "libtdjson.$VER.dylib")
n=\$#; i=0
while [ "\$i" -lt "\$n" ]; do
  a="\${A[\$i]}"
  case "\$a" in
    -Wl,-exported_symbols_list)
      out+=("-exported_symbols_list" "\${A[\$((i+1))]#-Wl,}"); i=\$((i+2)); continue ;;
    -Wl,*)
      IFS=',' read -ra parts <<< "\${a#-Wl,}"
      out+=("\${parts[@]}"); i=\$((i+1)); continue ;;
    -mmacosx-version-min=*)
      v="\${a#-mmacosx-version-min=}"
      out+=("-platform_version" "macos" "\$v" "\$v"); i=\$((i+1)); continue ;;
    -dynamiclib) i=\$((i+1)); continue ;;  # -dylib added below
    -nodefaultlibs|-g|-lc|-lm) i=\$((i+1)); continue ;;  # in libSystem
  esac
  out+=("\$a"); i=\$((i+1))
done
exec ld64.lld -dylib "\${out[@]}"
EOF
chmod +x "$WORK"/*.sh

# --- 2. libiconv.tbd (zig ships only libSystem.tbd) ----------------------
mkdir -p "$WORK/lib"
cat >"$WORK/lib/libiconv.tbd" <<EOF
--- !tapi-tbd
tbd-version: 4
targets: [ $TBDTARGET ]
install-name: '/usr/lib/libiconv.2.dylib'
current-version: 9.0.0
compatibility-version: 7.0.0
exports:
  - targets: [ $TBDTARGET ]
    symbols: [ _iconv, _iconv_open, _iconv_close, _iconvctl, _iconv_errno,
               __iconv, __iconv_free_list, __iconv_get_relocation_prefix ]
...
EOF

# --- 3. the build ---------------------------------------------------------
cd "$RS_ROOT"
env -u RUSTC_WRAPPER \
  CC_${TENV}="$WORK/cc.sh" \
  CXX_${TENV}="$WORK/cxx.sh" \
  AR_${TENV}="$WORK/ar.sh" \
  RANLIB="$WORK/ranlib.sh" RANLIB_${TENV}="$WORK/ranlib.sh" \
  CARGO_TARGET_$(echo "$TENV" | tr 'a-z' 'A-Z')_LINKER="$WORK/linker.sh" \
  CFLAGS_${TENV}="-DSQLCIPHER_OMIT_LOG_DEVICE" \
  CXXFLAGS_${TENV}="-DSQLCIPHER_OMIT_LOG_DEVICE" \
  rustup run "$TOOLCHAIN" cargo build --release --locked -p tdjson \
    --target "$TRIPLE"
DYLIB="target/$TRIPLE/release/libtdjson.dylib"

# --- 4. the package + static parity (llvm tools work on Mach-O) ----------
mkdir -p "$DIST/include/td/telegram" "$DIST/lib"
install -m 644 "$TD_CPP/td/telegram/td_json_client.h" "$DIST/include/td/telegram/"
install -m 644 "$TD_CPP/td/telegram/td_log.h" "$DIST/include/td/telegram/"
install -m 644 "$TD_CPP/build-d1/td/telegram/tdjson_export.h" \
  "$DIST/include/td/telegram/tdjson_export.h"
install -m 755 "$DYLIB" "$DIST/lib/libtdjson.$VER.dylib"
ln -sf "libtdjson.$VER.dylib" "$DIST/lib/libtdjson.dylib"

fail=0
llvm-nm -gU "$DIST/lib/libtdjson.dylib" | awk '{print $3}' | sort >"$WORK/exports"
if diff -u "$RS_ROOT/packaging/macos/exported-symbols.txt" "$WORK/exports"; then
  echo "PARITY_SYMBOLS_OK ($(wc -l <"$WORK/exports") exported)"
else
  echo "PARITY_SYMBOLS_DIFF"; fail=1
fi
for h in td_json_client.h td_log.h; do
  cmp -s "$TD_CPP/td/telegram/$h" "$DIST/include/td/telegram/$h" \
    && echo "PARITY_HEADER_OK $h" || { echo "PARITY_HEADER_DIFF $h"; fail=1; }
done
cmp -s "$TD_CPP/build-d1/td/telegram/tdjson_export.h" \
  "$DIST/include/td/telegram/tdjson_export.h" \
  && echo "PARITY_HEADER_OK tdjson_export.h" \
  || { echo "PARITY_HEADER_DIFF tdjson_export.h"; fail=1; }
ID=$(llvm-otool -D "$DIST/lib/libtdjson.dylib" | tail -1 | sed 's@.*/@@')
[ "$ID" = "libtdjson.$VER.dylib" ] && echo "PARITY_INSTALL_ID_OK $ID" \
  || { echo "PARITY_INSTALL_ID_BAD '$ID'"; fail=1; }
[ "$(readlink "$DIST/lib/libtdjson.dylib")" = "libtdjson.$VER.dylib" ] \
  && echo "PARITY_SYMLINK_OK" || { echo "PARITY_SYMLINK_BAD"; fail=1; }
# OS-only imports (the tdlib contract: nothing beyond the system)
llvm-otool -L "$DIST/lib/libtdjson.dylib" | tail -n +2 \
  | sed -e 's@ (.*@@' -e 's@^[[:space:]]*@@' \
  | grep -v -e '^/usr/lib/' -e '^libtdjson\.' \
  && { echo "PARITY_IMPORTS_BAD"; fail=1; } \
  || echo "PARITY_IMPORTS_OK (OS-only: libSystem + libiconv)"
[ "$fail" = 0 ] && echo "PACKAGED_OK $DIST (static parity; runtime needs a Mac)"
exit $fail
