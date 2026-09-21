# file_db_oracle (M.12.1h)

Pins the `FileData` records the C++ files kv stores (FileData.hpp
`store`/`parse` templates) for the Rust `files.rs` FileData store/parse.

- `./build.sh [td-root]` compiles `probe.cpp` and links the
  `$TD_ROOT/TD_BUILD` static libs (the full core set: FileData::parse
  instantiates the file-source registration plane even with
  `register_file_sources = false`, so the manager symbols must resolve).
- `oracle.out` — one `fd:<name>:<hex>` line per sample record (the
  `FileData::store` bytes) plus `rt:` parse -> re-store self-consistency
  lines and `cs:` `FileEncryptionKey::store` bytes.
- The hex bytes are pasted into
  `crates/tdclient/src/files.rs::file_data_matches_the_cpp_file_db_oracle`,
  which asserts Rust store == C++ store and parse -> re-store stability.

Samples carry no file sources: the source serialization needs the
FileReferenceManager context (the G() plane) and is covered by the
file_data_sources tests with the TestFileSources seam.
