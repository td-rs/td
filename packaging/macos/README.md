# macOS packaging — cross-built from Linux (zig + ld64.lld, no Apple SDK)

**Status: the macOS artifacts EXIST and pass static parity — built from the
Linux workstation.** `build-cross.sh <x86_64|aarch64> <dist>` compiles the
whole tdjson graph for macOS with zig's clean-room libSystem stubs as the
C/C++ toolchain and links the final dylib with raw `ld64.lld` (nix
`lld`+`llvmPackages.libllvm`), then assembles the release layout and runs
the static parity checks (14-symbol ABI vs `exported-symbols.txt`, header
byte-identity, install-id, symlink, OS-only imports). Shipped 2026-09-28:
`tdlib-rs-1.8.67-macos-x64` + `tdlib-rs-1.8.67-macos-arm64` (evidence
`/tmp/tdrs-macos-dist/`).

```sh
nix-shell -p rustup  # once: rustup run 1.95.0 rustup target add x86_64-apple-darwin aarch64-apple-darwin
packaging/macos/build-cross.sh x86_64  tdlib-rs-1.8.67-macos-x64
packaging/macos/build-cross.sh aarch64 tdlib-rs-1.8.67-macos-arm64
```

The script self-hosts its tool wrappers (CC/CXX/AR/RANLIB around `zig cc`,
the ld64.lld linker driver) and the hand-written `libiconv.tbd`. The four
walls it exists for (each cost a debugging round on 2026-09-28):

1. **zig cc DROPS `-exported_symbols_list`** (allowlisted linker args) — a
   zig-cc-linked dylib exports ~80k symbols (openssl et al.), breaking the
   drop-in ABI. The final link must run raw `ld64.lld`, with rustc's
   export list translated over — live, inside the linker-driver script
   (the list and codegen objects are rustc temps that vanish after).
2. **rustc emits the export list as two adjacent `-Wl` args** and the path
   arrives `-Wl,`-prefixed; ld64.lld wants `-exported_symbols_list <path>`.
3. **zig ships only `libSystem.tbd`** — `-liconv` (rustc's default darwin
   link arg) needs a written-by-hand `libiconv.tbd` describing the real
   system library (link- AND runtime-correct: install-name
   `/usr/lib/libiconv.2.dylib`).
4. Assorted: sqlcipher wants Apple's `os/log.h` (`-DSQLCIPHER_OMIT_LOG_DEVICE`,
   the supported escape hatch); cc-rs passes `--target=x86_64-apple-macosx`
   (zig rejects `macosx` — the wrappers drop incoming target flags); host
   `ranlib` mangles zig archives (`RANLIB` must be zig's).

## Runtime validation (the one thing a Mac is still good for)

Static parity is verified here (symbols, headers, layout, imports — the
same checks the linux/windows artifacts passed before their runtime
smokes). Actually LOADING the dylib needs a real darwin loader; there is
no wine-equivalent for macOS on Linux. On any Mac:

```sh
git clone <td-rs> && cd td-rs
# ad-hoc re-sign (the artifact is unsigned: fine for x86_64 native and
# under Rosetta; arm64-native macOS wants at least an ad-hoc signature):
codesign --force --sign - tdlib-rs-1.8.67-macos-*/lib/libtdjson.*.dylib
# the shipped C example against ONLY the package:
c++ example/cpp/tdjson_example.cpp -I<tdlib-rs-1.8.67-macos-x64>/include \
    -L<tdlib-rs-1.8.67-macos-x64>/lib -ltdjson && ./a.out
```

`make-release.sh`/`parity-check.sh` (written from the linux twins on
2026-09-27, before the cross existed) remain the ON-MAC path: build there
with cargo, re-package from a local C++ reference build, and diff symbol
sets against that reference — useful as an independent check, but the
cross-build above is the proven one.
