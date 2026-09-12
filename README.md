# td-rs

Rust rewrite of [TDLib](https://github.com/tdlib/td) (reference version 1.8.67) as a
drop-in replacement behind the existing 14-symbol C ABI (`td_json_client`), with
byte/JSON parity verified against the C++ client as a differential-testing oracle.
License: Boost 1.0.

The execution plan is `RUST_REWRITE_PLAN.md` at the root of the C++ repo
(`../td`). Progress is tracked as green, verified commits — one per plan step.

## Status

- [x] S01 — cargo workspace skeleton
- [x] S02 — CI skeleton: fmt + clippy `-D warnings` + `cargo test --workspace` + cargo-deny (verified locally; GitHub remote/first CI run pending)

## Checks (CI parity)

CI (`.github/workflows/ci.yml`) runs the same commands as these local nix
shells — keep them in sync when adding checks. `RUSTC_WRAPPER=` bypasses the
host sccache config, which is absent inside `nix-shell`:

```
nix-shell -p rustfmt --run 'cargo fmt --all --check'
nix-shell -p rustc cargo clippy rustfmt cargo-deny --run '
  RUSTC_WRAPPER= cargo clippy --workspace --all-targets --locked -- -D warnings &&
  RUSTC_WRAPPER= cargo test --workspace --locked &&
  RUSTC_WRAPPER= cargo deny --workspace --all-features check'
```
