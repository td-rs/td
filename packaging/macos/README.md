# macOS packaging — the documented toolchain gap (S83)

**Status: NOT BUILDABLE from the Linux workstation.** Apple's SDK license
permits building only on Apple hardware, the GitHub Actions macOS runners
are unreachable (billing — the standing S75/CI gap), and osxcross-style
cross-builds need an Apple-ID-licensed SDK download. This directory exists
so the artifact is **one command on any Mac** instead of archaeology:

```sh
# on the Mac (aarch64 or x86_64 — the artifact set is per-arch):
git clone <td-rs> && cd td-rs
cargo build --release --locked -p tdjson          # → target/release/libtdjson.dylib
cmake -S <td-cpp-checkout> -B <td-cpp-checkout>/build-mac && \
  cmake --build <td-cpp-checkout>/build-mac --target tdjson   # the reference + tdjson_export.h
TD_CPP=<td-cpp-checkout> packaging/macos/make-release.sh \
  target/release/libtdjson.dylib tdlib-rs-1.8.67-macos-$(uname -m)
TD_CPP=<td-cpp-checkout> packaging/macos/parity-check.sh \
  tdlib-rs-1.8.67-macos-$(uname -m)
```

- `make-release.sh` — assembles the official tdlib release-zip layout for
  macOS: `include/td/telegram/{td_json_client.h, td_log.h,
  tdjson_export.h}` + `lib/libtdjson.1.8.67.dylib` (install-id patched
  via `install_name_tool`, ad-hoc re-signed) + the
  `lib/libtdjson.dylib` symlink.
- `parity-check.sh` — the S83 audit for the mac artifact: exported symbol
  sets vs the C++ reference (`nm -gU`), byte-identity of all 3 headers,
  install-id + symlink + layout.

⚠ **Both scripts are UNTESTED-ON-MAC** — written 2026-09-28 from the
green linux twins (`packaging/linux/`), with the Linux-only steps
translated (`patchelf --set-soname` → `install_name_tool -id`,
`nm -D --defined-only` → `nm -gU`, `readelf -d` soname → `otool -D`) and
an ad-hoc `codesign` after the id patch (install_name_tool invalidates
the linker's signature). Expect small friction on first run; the linux
scripts are the reference for intent.

The tdjson_export.h subtlety carries over from the S83 windows finding:
the header is target-canonical (the cmake dllexport/dllimport variant is
windows-only; macOS uses the plain visibility form, same as linux).
