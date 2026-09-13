# conformance

Differential-testing harness against the C++ oracle:

- `vectors/` — golden data extracted from the C++ `test/data.cpp` base64 blobs
  (S04); regenerate with `cargo run -- <td-repo-root> vectors`.
- `cpp_dump/` — S04 verification oracle: a standalone C++ program that decodes
  the same embedded blobs the way the C++ tests do; `cpp_dump/verify.sh`
  byte-compares its output against `vectors/`.
- `tl_oracle/` — S06/S07/S08 verification oracles: compile td's unmodified
  `tl-parser.c` and diff its token stream (`verify.sh`), scheme-program dump
  (`verify_scheme.sh`) and `.tlo` serialization (`verify_tlo.sh`) against the
  Rust tdtl for all five schemes.
- `tl_store_oracle/` — S10 verification oracle: stores a sample set covering
  every wire form through td's own generated telegram_api C++ store methods
  (`TlStorerUnsafe`) and byte-compares against the tl-codegen-generated Rust
  store (`tdmtproto`'s `dump-telegram-store` bin); `tl_store_oracle/verify.sh`.
- `recorder/` — wraps the C++ client, logs requests/results/updates of real
  sessions (S73).
- `replayer/` — feeds the corpus to `tdjson` and diffs outputs (S74).

Standalone crate (excluded from the root workspace on purpose); run cargo
commands from this directory.
