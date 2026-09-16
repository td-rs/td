//! Fuzz target: the `SecretChat` database blob — the value every
//! `sc<id>` row and every SecretChats binlog event carries
//! (`UserManager::SecretChatLogEvent`, UserManager.cpp:2692-2714;
//! `SecretChat::parse`). Attacker-reachable from the database; the parse
//! gates the layer/ttl/state integers that later reach the secret-chat
//! crypto machine. Panic = bug: any byte string must parse or report an
//! error, never crash.

#![no_main]

use libfuzzer_sys::fuzz_target;

/// Silence the tdlib log: the binlog-replay rejects ("Failed to load a
/// secret chat from binlog") print per exec at fuzz rates. Level 0 keeps
/// only FATAL.
static QUIET_LOG: std::sync::Once = std::sync::Once::new();

fuzz_target!(|data: &[u8]| {
    QUIET_LOG.call_once(|| {
        tdutils::logging::set_verbosity_level(tdutils::logging::FATAL);
    });
    let _ = tdclient::user_manager::parse_secret_chat_log_event(data);
});
