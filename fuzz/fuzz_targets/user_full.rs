//! Fuzz target: the `UserFull` database blob — the value of every
//! `usf<id>` row (`UserManager::UserFull::parse`,
//! UserManager.cpp:2392-2470; the `log_event_store(*user_full)` framing
//! `save_user_full` writes). Attacker-reachable from the database like
//! the `User` blob; the unsupported-subset flags (photos, bot info,
//! business) must reject, not crash — the "can't happen unless database
//! is broken" repair branch of `on_load_user_full_from_database`
//! (UserManager.cpp:9374-9386) is the C++ answer to arbitrary bytes.

#![no_main]

use libfuzzer_sys::fuzz_target;

/// Silence the tdlib log: the repair-path `LOG(ERROR)` ("Repair broken
/// full") prints per exec at fuzz rates. Level 0 keeps only FATAL.
static QUIET_LOG: std::sync::Once = std::sync::Once::new();

fuzz_target!(|data: &[u8]| {
    QUIET_LOG.call_once(|| {
        tdutils::logging::set_verbosity_level(tdutils::logging::FATAL);
    });
    let ctx = tdclient::user_manager::TimeCtx {
        now: 1_000_000_000.0,
        server_time: 1_000_000_000.0,
    };
    let _ = tdclient::user_manager::UserFull::parse_value(data, &ctx);
});
