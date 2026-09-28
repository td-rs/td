# td-rs

**td-rs** is a from-scratch [Rust](https://www.rust-lang.org) rewrite of
[TDLib](https://github.com/tdlib/td) — the Telegram Database Library, the
cross-platform library for building Telegram clients. It is a **drop-in
replacement for the C++ `libtdjson` shared library**: it exports exactly the
same 14-function C JSON interface under the same names, ships byte-identical
copies of the official headers, and answers the same JSON — so existing TDLib
applications and bindings work unmodified. Currently tracking TDLib
**1.8.67**.

## Features

- **Drop-in C API.** The `tdjson` crate builds to a cdylib exporting exactly
  the 14 `td_json_client_*` / `td_*` symbols of the official interface
  (`td_json_client_create/send/receive/execute/destroy`, the modern
  `td_create_client_id`/`td_send`/`td_receive`/`td_execute`
  client-manager surface, and the `td_set_log_*` family) — nothing more, as
  audited against the C++ export list. The official `td_json_client.h` and
  `td_log.h` are vendored byte-identical under `crates/tdjson/include/`.
- **Verified parity.** Correctness is pinned against the C++ reference
  build, not reimplemented from memory: golden vectors extracted from the
  C++ test suite, byte-identical TL/binary serialization and binlog format,
  every one of the 1,022 `td_api` requests dispatched, exported-symbol and
  header parity checks over each release artifact, and live validation
  against production Telegram servers (login, configuration, messaging
  round-trips, file transfers).
- **All-Rust runtime.** The actor framework, MTProto transports and
  cryptography, the SQLCipher-backed database (vendored and compiled from
  source), and the networking (TCP/UDP, HTTP, proxies, WebSocket for the
  browser) are all Rust — no C++ is linked at runtime.
- **Cross-platform.** Release artifacts for Linux, Windows, WebAssembly, and
  macOS (see [Platform support](#platform-support)).
- **Cargo-native build.** `cargo build --release` is the whole story — no
  CMake, gperf, or PHP.

## Using

td-rs speaks the same JSON interface as TDLib, so the official
[TDLib documentation](https://core.telegram.org/tdlib/docs) and the
[`td_api.tl`](https://github.com/tdlib/td/blob/master/td/generate/scheme/td_api.tl)
scheme apply unchanged. From C:

```c
#include <td/telegram/td_json_client.h>

int main(void) {
    void *client = td_json_client_create();

    /* synchronous request — answered directly */
    const char *entities = td_json_client_execute(client,
        "{\"@type\":\"getTextEntities\",\"text\":\"@telegram /start https://telegram.org\"}");

    /* asynchronous request — the answer (and updates) arrive via receive */
    td_json_client_send(client, "{\"@type\":\"getOption\",\"name\":\"version\"}");
    const char *answer = td_json_client_receive(client, 10.0); /* 1.8.67 */

    td_json_client_destroy(client);
}
```

Build it against the td-rs `libtdjson` exactly as against the C++ one:

```sh
cc app.c -I<include-dir> -L<lib-dir> -ltdjson
```

Any language that can call C functions can use td-rs the same way it uses
TDLib — existing TDLib bindings for Python, Go, Node.js, and the rest work
unmodified.

## Building

Requirements:

- a Rust toolchain — 1.95, pinned in `rust-toolchain.toml`;
- a C compiler, `perl`, and `pkg-config` — the vendored OpenSSL and
  SQLCipher are compiled from source by the build, so no system copies are
  needed;
- zlib headers if available (`zlib1g-dev` on Ubuntu); otherwise zlib is
  compiled from source as a fallback.

```sh
git clone https://github.com/td-rs/td.git
cd td
cargo build --release -p tdjson
```

- Release artifacts (the tdlib release-zip layout, symbol/header parity
  checks, cross-builds) and the WebAssembly/tdweb build recipes:
  [packaging/](packaging/) and [RELEASE.md](RELEASE.md).

## Platform support

| Platform | Status |
| --- | --- |
| Linux x86-64 | release artifact; symbol + header + layout parity green |
| Windows x86-64 | cross-built (mingw-w64); symbol parity + Wine smoke green |
| WebAssembly (tdweb) | pthread module, live-validated in Chromium — full MTProto session, IndexedDB persistence |
| macOS x86-64 / aarch64 | cross-built artifacts; runtime validation on Apple hardware pending |

## Repository layout

The workspace mirrors the upstream module structure:

| Crate | Port of |
| --- | --- |
| `tdjson` | the C ABI — the 14-symbol JSON client cdylib |
| `tdclient` | `td/telegram` — client logic, managers, td_api queries and updates |
| `tdcli` | `td/telegram/cli.cpp` — the `tg_cli`-style command-line client |
| `tdmtproto` | `td/mtproto` — transports, KDF/IGE encryption, auth keys, sessions |
| `tde2e` | `td/e2e` — end-to-end (secret chat) encryption, encrypted storage |
| `tddb` | `td/db` — SqliteDb, binlog (byte-identical format), key-values |
| `tdnet` | `td/net` — TCP/UDP, DNS, HTTP, proxies, fake-TLS |
| `tdactor` | `td/actor` — the actor runtime |
| `tdutils` | `td/utils` — Status, Buffer, logging, crypto, time, JSON |
| `tdtl` | the TL scheme lexer/parser producing the scheme AST |
| `tl-codegen` | `td/generate` — Rust code generation from the TL schemes |

`conformance/` (differential testing against the C++ reference build),
`fuzz/`
(cargo-fuzz targets), `bench/` (benchmarks vs the C++ build), and
`packaging/` (release artifacts) are standalone crates excluded from the
workspace.

## Development

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
RUST_MIN_STACK=16777216 cargo test --workspace --locked
```

`RUST_MIN_STACK` is required: a few tests recurse deeply in debug builds.
CI (`.github/workflows/`) runs the same checks plus a platform matrix,
sanitizers, perf gating, and release builds.

## License

td-rs is licensed under the [MIT License](LICENSE). The rewrite derives
from [TDLib](https://github.com/tdlib/td), which is licensed under the
Boost Software License 1.0. One exception: the build-time `tdtl` crate is a
faithful port of TDLib's GPL-2.0-or-later tl-parser and carries that
license — it is used for code generation only and is never linked into the
shipped library.
