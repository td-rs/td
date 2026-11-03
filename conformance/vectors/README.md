# Golden vectors from C++ td `test/data.cpp`

Extracted by plan step S04; see `RUST_REWRITE_PLAN.md` in the C++ repo.

- Source file: `test/data.cpp` in the C++ td repository
- Source commit: `2a26e010767a5d65fd68813d64efcfa6bbcfbc1c`
- Regenerate: from `conformance/`, `cargo run -- <td-repo-root> vectors`

Integrity: `sha256sum -c sha256sums.txt`

Verification (S04): bytes must equal the output of the standalone C++ dump
oracle in `../cpp_dump/` (`conformance/cpp_dump/verify.sh`), which decodes
the same embedded arrays the C++ tests consume.

| file | symbol in data.cpp | bytes | sha256 | variant | decoded by C++ tests |
|------|--------------------|-------|--------|---------|----------------------|
| `thumbnail.bin` | `thumbnail` | 2105 | `c4d043bb8d49eea5a9bac9bf4cd46d61585cee66b27ba2e2f4b85d4352736bf0` | base64url | test/tdclient.cpp `base64url_decode` |
| `gzip_bomb.bin` | `gzip_bomb` | 1799 | `86fe009b78e09774bd6ac09b1d31cdbdfd9d6c02e5d9a9a477fbeebdf4e57874` | base64url | test/http.cpp `base64url_decode` |
| `gzip.bin` | `gzip` | 267 | `b30ea80eaf74269da2e30bfd7c1c7c2fcb26aa03872c7eef61abf95cd4d62c1c` | base64url | test/http.cpp `base64url_decode` |
| `sqlite_sample_db_v3.bin` | `sqlite_sample_db_v3` | 5120 | `df5b3e4801cbbecb76fddf0fad0609ab2e8b822e671876f81783703af81d6e89` | base64 | test/db.cpp `base64_decode` (SQLCipher 3) |
| `sqlite_sample_db_v4.bin` | `sqlite_sample_db_v4` | 20480 | `af77e99fac9f58471afee71b015a24bc0be5273c7f303a2c259f93cb1e21c14e` | base64 | test/db.cpp `base64_decode` (SQLCipher 4) |
