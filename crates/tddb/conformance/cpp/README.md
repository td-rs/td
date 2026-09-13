# tddb C++ conformance tools

Cross-verification between the Rust binlog port and the C++ reference
(tdlib 1.8.67, the repo this project mirrors). Used for the S40 gate:
"C++ binlog file parses in Rust; Rust file parses in C++".

## Build the C++ side

Configure a TDLib build tree once (out-of-source), e.g.:

```sh
mkdir /tmp/td-build && cd /tmp/td-build
cmake -DCMAKE_BUILD_TYPE=Release \
  -DZLIB_INCLUDE_DIR=<zlib-dev>/include -DZLIB_LIBRARY=<zlib>/lib/libz.so \
  -DOPENSSL_ROOT_DIR=<openssl> -DOPENSSL_INCLUDE_DIR=<openssl-dev>/include \
  -DOPENSSL_SSL_LIBRARY=<openssl>/lib/libssl.so \
  -DOPENSSL_CRYPTO_LIBRARY=<openssl>/lib/libcrypto.so \
  ~/Projects/td
make -j tddb
```

Then compile the tools (adjust paths):

```sh
g++ -O2 -std=c++17 \
  -I ~/Projects/td -I ~/Projects/td/tddb \
  -I /tmp/td-build/tdutils -I /tmp/td-build \
  binlog_write_sample.cpp \
  /tmp/td-build/tddb/libtddb.a /tmp/td-build/tdactor/libtdactor.a \
  /tmp/td-build/tdutils/libtdutils.a /tmp/td-build/sqlite/libtdsqlite.a \
  -lpthread -ldl -lz -o binlog_write_sample
# same for binlog_read_sample.cpp
```

## C++ → Rust

```sh
./binlog_write_sample sample_cpp.binlog
# committed fixture: crates/tddb/tests/data/sample_cpp.binlog
cargo test -p tddb binlog_parse_cpp_sample
```

## Rust → C++

```sh
cargo run -p tddb --example write_sample_binlog -- /tmp/sample_rs.binlog
./binlog_read_sample /tmp/sample_rs.binlog
# output must equal the Rust replay listing
```
