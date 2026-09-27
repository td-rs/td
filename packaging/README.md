# packaging/ — release artifact tooling (S82/S83)

Everything needed to build, assemble, and parity-check the drop-in tdjson
release artifacts from this tree. Provenance: the S82 sessions (2026-09-27);
every script here ran green against production Telegram or the C++ reference
build during those sessions. See `wasm/README.md` for the full engineering
record (walls found, root causes, runtime proofs).

## linux-x64 (`linux/`)

- `make-release.sh <release-cdylib> <out-dir>` — assembles the official
  tdlib release-zip layout: `include/td/telegram/{td_json_client.h,
  td_log.h, tdjson_export.h}` (byte-copies of the C++ originals + the
  cmake-generated export header) + `lib/libtdjson.so.1.8.67` (SONAME patched
  via patchelf) + the `lib/libtdjson.so` symlink.
- `parity-check.sh <dist-dir>` — the S83 audit for the linux artifact:
  exported symbol sets vs the C++ reference build (exactly the 14 C-ABI
  functions), byte-identity of all 3 headers, soname + symlink + layout.

Env: `TD_CPP` (the C++ tdlib checkout, default `~/Projects/td`) must carry a
configured build dir `build-d1` with the cmake-generated `tdjson_export.h`
and the reference `libtdjson.so`.

## wasm / tdweb (`wasm/`)

`wasm/README.md` is the canonical recipe + findings log (the emcc hand-link,
the nightly `-Z build-std` pthread link, sqlcipher-over-libtomcrypt, the
zlib prune, the browser harness, the origin-gated `*.web.telegram.org`
finding and the ws-bridge dev seam).

- `build-*.sh` / `link-*.sh` — the staticlib build and emcc module links
  (probe, release, live, transport). Paths parameterized:
  `TD_WASM_TREE` (the td-rs worktree, default the S82 session worktree
  `/tmp/tdrs-s82-rel`) and `LTC_HEADERS` (libtomcrypt headers, default
  `/tmp/ltc/src/headers`).
- `smoke-*.cjs`, `deep-node2.cjs` — Node runtime smokes (module load +
  `td_execute`, the pthread startup milestone, the deep post-parameters
  chain; the receive loop must await between `td_receive` calls — the
  emscripten main-thread syscall-proxy contract).
- `sqlcipher-test.c`, `zlib-test.c`, `wal-test*.c` — the C probes that
  pinned the sqlcipher-libtomcrypt codec, the dead libz-sys archives, and
  the proxied-syscall WAL deadlock.
- `tdweb/` — the real-browser harness (COOP/COEP server, worker pages,
  CDP drivers) that ran the smoke/deep/IDB legs in Chromium.
- `bridge/` — the dev-seam transports for live testing: the ws↔TCP bridge
  (`*.web.telegram.org/apiws` is origin-gated to official clients, so wasm
  live legs route through it), the reverse bridge (native-through-ws
  differential), the TCP proxy (wire capture), the echo server, and the
  modpow differential probe.

## Platform matrix (S82 close)

- **linux-x64**: release cdylib + package + parity — PASS (see `linux/`).
- **wasm32-emscripten (tdweb)**: pthread release module — PASS live
  (handshake + config over the encrypted session, IDBFS persistence).
- **windows-x64**: cross-built via mingw-w64 (`pkgsCross.mingwW64` +
  `x86_64-pc-windows-gnu`); see the S83 record for the artifact + symbol
  parity evidence.
- **macos**: not buildable from this Linux toolchain (needs the Apple SDK);
  documented gap, mirrors the S81 platform gaps.
