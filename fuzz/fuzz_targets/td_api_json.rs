//! Fuzz target: the `td_execute` request parser — arbitrary strings through
//! `ClientJson::to_request` (JSON decode + `@extra` extraction + td_api
//! `Function` deserialization). Panic = bug: any input must yield a
//! function or an error function, never a crash.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(request) = std::str::from_utf8(data) {
        let _ = tdclient::client_json::to_request(request);
    }
});
