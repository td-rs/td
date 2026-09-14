//! Fuzz target: the raw tdlib-style JSON parser (`tdutils::json`), the same
//! parser `to_request` sits on, fed bytes directly (non-UTF-8 included).

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(text) = std::str::from_utf8(data) {
        let _ = tdutils::json::json_decode(text);
    }
});
