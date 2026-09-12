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
- [x] S03 — TL schemes imported from tdlib `d1085f9ce`, pinned by hash
- [x] S04 — golden vectors extracted from C++ `test/data.cpp` into `conformance/vectors/`, byte-verified against td's own `base64.cpp`
- [x] S05 — tdlib test harness (`td/utils/tests`) ported to `tdutils` (runner, timing, ctor registration, regression tester, stage barriers)
- [x] S06 — tl-parser lexer ported to `tdtl`, token-identical to the C original for all five schemes
- [x] S07 — tl-parser parser ported to `tdtl` (parse tree + scheme program with CRC32 constructor ids); scheme dump identical to the C oracle for all five schemes (td_api: 753 types / 2192 ctors / 1022 funcs)
- [x] S08 — `.tlo` serialization (`write_types`) ported to `tdtl`; byte-identical to td's own tl-parser output (`tl-parser -e`) for all five schemes, incl. combinator argument trees
- [x] S09 — `tl_writer_rust` part 1: the td_api data model generates from build.rs (3205 structs + 211 enums + the `Function` umbrella, serde derives, `Box` indirection on recursive types) and compiles; counts match the C-oracle scheme counts (753/2192/1022)

## Checks (CI parity)

CI lives in `.github/workflows/`:

- `ci.yml` — fast PR validation: fmt, clippy `-D warnings`, workspace tests,
  rustdoc, conformance build, cargo-deny. Also reused by Main CI on master.
- `main.yml` — master/nightly: all of `ci.yml` plus the platform matrix
  (Linux x64/arm64, macOS arm64 + Intel cross-check, Windows x64), a
  current-stable job, a wasm advisory check, a stress soak, and the C++
  oracle conformance verification.
- `sanitizers.yml` — ASan+LSan, TSan, Miri on a date-pinned nightly.
- `perf.yml` — clean build/test metrics gated against the committed baseline
  (`.github/perf-baseline.json`), plus benchmarks once `bench/` lands (S79).
- `release.yml` — `v*` tags: full-platform release builds, tests, artifacts.

The toolchain is pinned to the clippy MSRV (`rust-toolchain.toml`, `1.95`);
CI passes it explicitly in every job, and Main CI additionally tests current
stable. CI runs the same commands as these local nix shells — keep them in
sync when adding checks. `RUSTC_WRAPPER=` bypasses the host sccache config,
which is absent inside `nix-shell`:

```
nix-shell -p rustfmt --run 'cargo fmt --all --check'
nix-shell -p rustc cargo clippy rustfmt cargo-deny --run '
  RUSTC_WRAPPER= cargo clippy --workspace --all-targets --locked -- -D warnings &&
  RUSTC_WRAPPER= cargo test --workspace --locked &&
  RUSTC_WRAPPER= cargo doc --workspace --no-deps --locked &&
  RUSTC_WRAPPER= cargo deny --workspace --all-features check'
```
