//! S15 verification: dumps the deterministic replay corpus for tdmtproto's
//! three wire schemes — one `<tag> <T|F> <ctor-id> <hex>` line per sample
//! (two depths per constructor and function). The C++ replay oracle
//! (`conformance/tl_replay_oracle/replay.cpp`) fetches every blob with td's
//! own generated parser and re-stores it; the bytes must be identical.
//!
//! Build with `--features conformance-samples` (the corpus builders are
//! otherwise compiled only under cfg(test)).

#[cfg(feature = "conformance-samples")]
fn main() {
    tdmtproto::telegram_api::samples::__replay_dump("tg");
    tdmtproto::secret_api::samples::__replay_dump("sec");
    tdmtproto::mtproto_api::samples::__replay_dump("mt");
}

#[cfg(not(feature = "conformance-samples"))]
fn main() {
    eprintln!("build with --features conformance-samples");
    std::process::exit(2);
}
