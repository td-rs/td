# mtproto_transport_oracle (S54)

Differential gate for the MTProto stream transports: td's own
`IntermediateTransport` (`td/mtproto/TcpTransport.cpp`) and the Rust port
(`tdmtproto::transport::tcp::IntermediateTransport`) run the same scenarios
and must produce byte-identical output.

- `cpp/dump_transport.cpp` — the C++ side, compiled against td sources and
  the prebuilt `libtdutils.a` (build tdlib once with cmake; point `TD_BUILD`
  at the build directory).
- `crates/tdmtproto/src/bin/dump_transport.rs` — the Rust mirror.
- `verify.sh` — builds both, diffs the deterministic abridged matrix
  (writes/reads × payloads × quick-ack, quick-ack-only frames, partial
  frames, empty streams) and cross-reads random-padded frames written by
  each side.
