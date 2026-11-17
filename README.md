# td-rs

A from-scratch [Rust](https://www.rust-lang.org) rewrite of
[TDLib](https://github.com/tdlib/td) — the Telegram Database Library. td-rs
is a **drop-in replacement for TDLib**: it exports exactly the same
14-function C JSON interface under the same names, ships byte-identical
copies of the official headers, and answers the same JSON — so existing
TDLib applications and bindings work unmodified. Tracks TDLib **1.8.70**.

## Status

Implemented and verified:

- The complete C JSON interface: `td_json_client_create/send/receive/
  execute/destroy`, the `td_create_client_id`/`td_send`/`td_receive`/
  `td_execute` client-manager surface, and the `td_set_log_*` family —
  exactly the 14 official symbols, audited against the C++ export list on
  every release artifact.
- Byte-identical TL and binary serialization; binlog and SQLite databases
  are interchangeable with the C++ build in both directions.
- MTProto: transports, handshake and auth-key management, sessions,
  KDF/IGE encryption.
- Networking: TCP/UDP, DNS, HTTP, SOCKS5 and MTProxy proxies, fake-TLS,
  WebSocket for the browser.
- SQLCipher-encrypted storage (vendored, compiled from source), the actor
  runtime, file downloads, notifications, secret chats.
- All td_api requests of the 1.8.70 scheme are dispatched to handlers
  (audited by `requests_gate/`).
- Live validation against production Telegram servers: login,
  configuration, message round-trips, file transfers.
- Differential conformance suite against the C++ reference build
  (`conformance/`), cargo-fuzz targets (`fuzz/`), and benchmarks against
  the C++ build (`bench/`).

The client is Rust end to end — no C++ is linked at runtime. The vendored
C dependencies are OpenSSL, SQLCipher, and zlib.

In progress:

- A tail of request families whose network handlers are not yet ported —
  288 `[blocked: net]` markers in `crates/` as of 2026-11-09. Affected
  requests are accepted but stay unanswered. Tracked in
  [docs/REMAINING_WORK.md](docs/REMAINING_WORK.md) and the issue tracker.
- `wasm32-unknown-unknown` is not a target (no OS for the tddb/sqlcipher C
  stack); `wasm32-unknown-emscripten` is the supported wasm target — Main
  CI's advisory `wasm` job checks it (`cargo check --lib` with the pinned
  emsdk).

### Platform support

| Platform | Status |
| --- | --- |
| Linux x86-64 | release artifact; symbol, header and layout parity green |
| Windows x86-64 | cross-built (mingw-w64); symbol parity + Wine smoke test |
| macOS x86-64 / aarch64 | cross-built (zig + ld64.lld); static parity; x86-64 runtime-validated on macOS 14 |
| wasm32-unknown-emscripten (tdweb) | release module via `packaging/wasm`; live-validated in Chromium — full MTProto session, IndexedDB persistence |

## Building

Requirements:

- Rust 1.95 (pinned in `rust-toolchain.toml`);
- a C compiler, `perl`, and `pkg-config` — vendored OpenSSL and SQLCipher
  are compiled from source, so no system copies are needed;
- zlib headers (`zlib1g-dev` on Ubuntu) if available; otherwise zlib is
  compiled from source.

```sh
git clone https://github.com/td-rs/td.git
cd td
cargo build --release -p tdjson
```

The build produces `libtdjson.so` under `target/release/`. Release
packaging — the official tdlib release-zip layout, symbol/header parity
checks, cross-builds, and the wasm/tdweb recipes — lives in
[packaging/](packaging/) (see also [RELEASE.md](RELEASE.md)).

## Configuration

td-rs has no configuration files or environment variables of its own.
Clients configure it through the standard td_api requests
(`setTdLibParameters`, `setOption`), exactly as with C++ TDLib.

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

    /* asynchronous request — the answer arrives via receive */
    td_json_client_send(client, "{\"@type\":\"getOption\",\"name\":\"version\"}");
    const char *answer = td_json_client_receive(client, 10.0); /* 1.8.70 */

    td_json_client_destroy(client);
}
```

Link against the td-rs build exactly as against the C++ one:

```sh
cc app.c -I<include-dir> -L<lib-dir> -ltdjson
```

The official headers are vendored under `crates/tdjson/include/`. Any
language that can call C functions can use td-rs the same way it uses
TDLib — existing TDLib bindings work unmodified.

For interactive testing, `tdcli` builds the `tg_cli`-style command-line
client (binary `td_cli`):

```sh
cargo run -p tdcli
```

## Repository layout

The workspace mirrors the upstream td module structure:

| Crate | Port of |
| --- | --- |
| `tdjson` | the C ABI — the 14-symbol JSON client interface |
| `tdclient` | `td/telegram` — client logic, managers, td_api requests and updates |
| `tdcli` | `td/telegram/cli.cpp` — the `tg_cli`-style command-line client |
| `tdmtproto` | `td/mtproto` — transports, KDF/IGE encryption, auth keys, sessions |
| `tde2e` | `td/e2e` — secret-chat encryption and encrypted storage |
| `tddb` | `td/db` — SqliteDb, binlog (byte-identical format), key-value storage |
| `tdnet` | `td/net` — TCP/UDP, DNS, HTTP, proxies, fake-TLS |
| `tdactor` | `td/actor` — the actor runtime |
| `tdutils` | `td/utils` — Status, Buffer, logging, crypto, time, JSON |
| `tdtl` | the TL scheme lexer/parser |
| `tl-codegen` | `td/generate` — Rust code generation from the TL schemes |

Scheme code is committed under `crates/*/src/generated/` and compiled
directly; `tl-codegen` is a development tool, not a build dependency.

`conformance/` (differential testing against the C++ reference build),
`fuzz/` (cargo-fuzz targets), `bench/` (benchmarks against the C++ build),
and `packaging/` (release artifacts) are standalone crates excluded from
the workspace.

## Development

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
RUSTDOCFLAGS=-D warnings cargo doc --workspace --no-deps --locked
cargo test --workspace --locked
```

`.cargo/config.toml` sets `RUST_MIN_STACK` for all cargo-spawned processes;
set it manually only when running a test binary directly. CI
(`.github/workflows/`) runs the same suite on every PR and master push,
plus a platform matrix, sanitizers, a perf gate, and — on master — the
differential conformance gates against the C++ reference build (requires a
C++ td checkout and build; see `conformance/README.md`).

## License

td-rs is licensed under the [MIT License](LICENSE). The rewrite derives
from [TDLib](https://github.com/tdlib/td), which is licensed under the
Boost Software License 1.0. One exception: the build-time `tdtl` crate is
a faithful port of TDLib's GPL-2.0-or-later tl-parser and carries that
license — it is used for code generation only and is never linked into
the release artifacts.
