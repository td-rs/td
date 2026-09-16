//! Fuzz target: the `EmojiStatus` value parser (EmojiStatus.h) — the
//! reduced payload stored inside `User` blobs and userFull records (the
//! M.4.11 helper the `get_user_object` builder shares). Panic = bug: any
//! byte string must parse or report an error, never crash.

#![no_main]

use libfuzzer_sys::fuzz_target;

/// Silence the tdlib log (uniform with the other S78 targets). Level 0
/// keeps only FATAL.
static QUIET_LOG: std::sync::Once = std::sync::Once::new();

fuzz_target!(|data: &[u8]| {
    QUIET_LOG.call_once(|| {
        tdutils::logging::set_verbosity_level(tdutils::logging::FATAL);
    });
    let _ = tdclient::emoji_status::EmojiStatus::parse(data);
});
