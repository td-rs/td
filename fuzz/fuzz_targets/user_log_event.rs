//! Fuzz target: the `User` database blob — the value every `us<id>` row
//! and every Users binlog event carries (`UserManager::UserLogEvent`,
//! UserManager.cpp:2668-2690; `User::parse`, UserManager.cpp:1965-2175).
//! These bytes are attacker-reachable: they persist on disk and replay on
//! every client start.
//!
//! Input layout: one leading flag byte (bit 0 selects the framing — 0 is
//! the binlog/`UserLogEvent` form version+user_id+`User`, 1 is the
//! `us<id>` row form version+`User`), the rest is the blob. Panic = bug:
//! any byte string must parse or report an error, never crash; the C++
//! sanity rejects (`invalid first name`, `invalid flag is_mutual_contact`,
//! …) degrade to `cache_version = 0`, not a failure.

#![no_main]

use libfuzzer_sys::fuzz_target;

/// Silence the tdlib log: the C++-parity rejects ("Have invalid first
/// name", "Failed to load a user from binlog", …) print per exec at fuzz
/// rates and would drown the campaign log. Level 0 keeps only FATAL.
static QUIET_LOG: std::sync::Once = std::sync::Once::new();

fuzz_target!(|data: &[u8]| {
    QUIET_LOG.call_once(|| {
        tdutils::logging::set_verbosity_level(tdutils::logging::FATAL);
    });
    let Some((&framing, blob)) = data.split_first() else {
        return;
    };
    // A fixed clock bundle: the parse only rebases stored deadlines
    // against `now`/`server_time`, so any finite values are equivalent.
    let ctx = tdclient::user_manager::TimeCtx {
        now: 1_000_000_000.0,
        server_time: 1_000_000_000.0,
    };
    if (framing & 0x01) == 0 {
        let _ = tdclient::user_manager::parse_user_log_event(blob, &ctx);
    } else {
        let _ = tdclient::user_manager::User::parse_value(blob, &ctx);
    }
});
