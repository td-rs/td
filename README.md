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
- [x] S10 — TL binary store: primitives, TL string framing (all three length forms), bare/boxed vectors, the flags word with auto-OR'd `?true` bits, conditional fields, bare/boxed objects; telegram_api model generated into `tdmtproto`, byte-identical to td's generated C++ `TlStorerUnsafe` on the oracle sample set (`conformance/tl_store_oracle`); generated round-trip + malformed-input tests included
- [x] S11 — TL binary parse: bounds-checked `tl::Reader` mirroring td's `TlBufferParser` ("Not enough data to read", "Bool expected", "Wrong vector length", negative-`#` rejection, string `\0`→space + UTF-8 truncation, raw bytes); per-ctor store→parse round-trip tests over every type and function of both generated schemes (boxed + bare), malformed-input seeds that must error and never panic, and depth-capped sample synthesizers with proven termination (landed with the S10 commit; this step marks completion and preserves test panic payloads across the big-stack wrapper)
- [x] S12 — td_api JSON: generated `to_json`/`from_json` over the whole scheme, byte-identical to td's own `td_api_json_*.cpp` on the oracle sample set (`conformance/tl_json_oracle`: every emission form incl. int64-as-quoted-string, fixed-6-decimal doubles, full `\uXXXX` escape range, base64 bytes, quoted `vector<int64>` elements, plus lenient request parsing with identical error messages). Known divergence: td omits null `object_ptr` fields; the Rust model materializes defaults, so object fields are always emitted (binary store/parse never encodes null either — revisit with the dynamic JSON layer, S16/S68). Per-ctor JSON round-trip tests and malformed-JSON seeds ride along in the generated module
- [x] S13 — writers for the remaining schemes: secret_api (20/94/1) and mtproto_api (33/40/8) generate into `tdmtproto`, e2e_api (36/48/1) into `tde2e`, each with the full binary store/parse + per-ctor round-trip + malformed-input tests; scheme counts pinned against the S07 differential dump. The anonymous `4*[ int ]` multiplicity args of `int128`/`int256` map to a named `words: Vec<i32>` field (wire-identical: 4 little-endian int32s); builtin-ctor accounting is now computed per scheme (each declares a different builtin subset)
- [x] S14 — build integration complete: every scheme generates through `build.rs` into `OUT_DIR` (regeneration keyed on the tl-codegen sources), verified by a cold `cargo clean` → `cargo build --workspace --locked` (2m25s from nothing, all five schemes). SplitSource decision: td splits its generated C++ into ten `.cpp`s because its parallelism unit is the file; Rust's is the crate, so each scheme stays one module and the `#[cfg(test)]` samples/tests compile out of non-test builds — revisit only if generated-code typeck ever dominates build time (numbers in the S17 baseline)

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
