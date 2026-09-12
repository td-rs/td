# conformance

Differential-testing harness against the C++ oracle:

- `vectors/` — golden data extracted from the C++ `test/data.cpp` base64 blobs
  (S04); regenerate with `cargo run -- <td-repo-root> vectors`.
- `cpp_dump/` — S04 verification oracle: a standalone C++ program that decodes
  the same embedded blobs the way the C++ tests do; `cpp_dump/verify.sh`
  byte-compares its output against `vectors/`.
- `tl_oracle/` — S06 verification oracle: compiles td's unmodified
  `tl-parser.c` and diffs its token stream against the Rust tdtl lexer
  (`tl-token-dump`) for all five schemes; `tl_oracle/verify.sh`.
- `recorder/` — wraps the C++ client, logs requests/results/updates of real
  sessions (S73).
- `replayer/` — feeds the corpus to `tdjson` and diffs outputs (S74).

Standalone crate (excluded from the root workspace on purpose); run cargo
commands from this directory.
