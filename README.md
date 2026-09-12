# td-rs

Rust rewrite of [TDLib](https://github.com/tdlib/td) (reference version 1.8.67) as a
drop-in replacement behind the existing 14-symbol C ABI (`td_json_client`), with
byte/JSON parity verified against the C++ client as a differential-testing oracle.
License: Boost 1.0.

The execution plan is `RUST_REWRITE_PLAN.md` at the root of the C++ repo
(`../td`). Progress is tracked as green, verified commits — one per plan step.

## Status

- [x] S01 — cargo workspace skeleton
