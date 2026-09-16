//! Fuzz target: the simple-config decoder (`decode_config`,
//! ConfigManager.cpp:84-141) — arbitrary text through the length gate, the
//! base64 filter, the RSA signature decrypt, the AES-CBC decrypt and the
//! SHA-256 check down to the TL `help.configSimple` parse. Panic = bug: every
//! rejection must be an error status, never a crash (the happy path is pinned
//! byte-exact by the M.3.6 unit tests; the fuzzer owns the rejection space).

#![no_main]

use libfuzzer_sys::fuzz_target;

/// Silence the tdlib log: every rejected candidate logs `Invalid
/// [length:…]`-style errors, which prints per exec at fuzz rates. Level 0
/// keeps only FATAL (the gate is `log_level <= verbosity_level`).
static QUIET_LOG: std::sync::Once = std::sync::Once::new();

fuzz_target!(|data: &[u8]| {
    QUIET_LOG.call_once(|| {
        tdutils::logging::set_verbosity_level(tdutils::logging::FATAL);
    });
    if let Ok(text) = std::str::from_utf8(data) {
        let _ = tdclient::config_recoverer::decode_config(text);
    }
});
