//! Fuzz target: the UpdatesManager engine (M.7) — arbitrary `Updates`
//! umbrellas and `updates_Difference` responses through the full engine:
//! the `on_get_updates` routing (all umbrella variants), the pts/qts/seq
//! pending + postponed queues, the gap timers, the acceptability filter
//! and the getDifference application, against a deterministic environment
//! (clock, options, binlog pmc) and the `[blocked: net]` sender seams.
//!
//! Input layout: one leading flag byte (bit 0 `is_authorized`, bit 1
//! `pts_state_seeded`, bit 2 `drive_difference`), then up to four
//! length-prefixed (`u32` BE) serialized `Updates` umbrellas, the
//! remainder (with flag 2) is the `updates_Difference` response. Panic =
//! bug: any wire-shaped input may advance the state or trip the
//! enumerated trace drops, never crash (the C++ engine is no-throw on
//! wire-shaped input; the `CHECK`s of cpp became documented rejects).

#![no_main]

use libfuzzer_sys::fuzz_target;
use tdclient::updates_manager::{PendingUpdate, UpdatesManager, UpdatesManagerEnv, UpdatesSink};
use tdmtproto::telegram_api;
use tdutils::promise::Promise;

#[derive(Default)]
struct NullSink;

impl UpdatesSink for NullSink {
    fn on_user(&mut self, _user: telegram_api::User) {}
    fn on_chat(&mut self, _chat: telegram_api::Chat) {}
    fn process_pts_update(&mut self, _update: PendingUpdate) {}
    fn add_pending_channel_update(
        &mut self,
        _peer: Option<telegram_api::Peer>,
        _update: PendingUpdate,
        _pts: i32,
        _pts_count: i32,
        _promise: Promise<()>,
        _source: &str,
    ) {
    }
}

#[derive(Default)]
struct FuzzEnv {
    clock: f64,
    pmc: std::collections::BTreeMap<String, String>,
    options: std::collections::BTreeMap<String, i64>,
    authorized: bool,
}

impl UpdatesManagerEnv for FuzzEnv {
    fn now(&mut self) -> f64 {
        self.clock
    }

    fn unix_time(&mut self) -> i32 {
        self.clock as i32
    }

    fn close_flag(&mut self) -> bool {
        false
    }

    fn is_authorized(&mut self) -> bool {
        self.authorized
    }

    fn is_bot(&mut self) -> bool {
        false
    }

    fn ignore_background_updates(&mut self) -> bool {
        false
    }

    fn can_ignore_background_updates(&mut self) -> bool {
        false
    }

    fn get_option_integer(&mut self, name: &str) -> i64 {
        self.options.get(name).copied().unwrap_or(0)
    }

    fn set_option_empty(&mut self, _name: &str) {}

    fn pmc_get(&mut self, key: &str) -> String {
        self.pmc.get(key).cloned().unwrap_or_default()
    }

    fn pmc_set(&mut self, key: &str, value: &str) {
        self.pmc.insert(key.to_string(), value.to_string());
    }

    fn pmc_erase(&mut self, key: &str) {
        self.pmc.remove(key);
    }

    fn get_my_id(&mut self) -> i64 {
        7
    }

    fn send_get_difference(&mut self, _pts: i32, _date: i32, _qts: i32) {}

    fn send_get_updates_state(&mut self) {}

    fn send_confirm_pts_qts(&mut self, _pts: i32, _qts: i32) {}

    fn send_get_pts_update(&mut self, _pts: i32) {}

    fn send_ping_server(&mut self) {}

    fn is_online(&mut self) -> bool {
        false
    }
}

/// Silence the tdlib log: the C++-parity `LOG(ERROR)` rejects ("Unsupported
/// update …", "Receive wrong PTS update …") print per exec at fuzz rates.
static QUIET_LOG: std::sync::Once = std::sync::Once::new();

fuzz_target!(|data: &[u8]| {
    QUIET_LOG.call_once(|| {
        tdutils::logging::set_verbosity_level(tdutils::logging::FATAL);
    });
    let Some((&flags, rest)) = data.split_first() else {
        return;
    };
    let mut env = FuzzEnv {
        authorized: flags & 0x01 != 0,
        ..FuzzEnv::default()
    };
    if flags & 0x02 != 0 {
        // A pre-seeded pmc state: the lazy `init_state` restore paths run
        // against real (possibly hostile) persisted values.
        env.pmc.insert("updates.pts".to_string(), "5".to_string());
        env.pmc.insert("updates.qts".to_string(), "3".to_string());
        env.pmc.insert("updates.date".to_string(), "1".to_string());
        env.pmc.insert("updates.seq".to_string(), "2".to_string());
    }

    let mut manager: UpdatesManager<NullSink> = UpdatesManager::new(NullSink);
    let mut cursor = rest;

    // Up to four umbrellas per input, each length-prefixed (`u32` BE),
    // with a clock tick between them so the timers participate.
    for _ in 0..4 {
        let Some(len) = read_len(&mut cursor) else {
            break;
        };
        if len == 0 || len > cursor.len() {
            break;
        }
        let (chunk, remainder) = cursor.split_at(len);
        cursor = remainder;
        if let Ok(updates) = telegram_api::Updates::parse(chunk) {
            manager.on_get_updates(&mut env, updates, Promise::default());
        }
        env.clock += 0.3;
        manager.tick_timeouts(&mut env);
    }

    // The difference response drives the getDifference intake directly
    // (the S74 harness convention: the sender is `[blocked: net]`, the
    // response intake is public).
    if flags & 0x04 != 0
        && let Ok(difference) = telegram_api::updates_Difference::parse(cursor)
    {
        manager.on_get_difference(&mut env, difference);
        env.clock += 0.3;
        manager.tick_timeouts(&mut env);
    }
});

/// The `u32` BE length prefix reader.
fn read_len(cursor: &mut &[u8]) -> Option<usize> {
    if cursor.len() < 4 {
        return None;
    }
    let (bytes, remainder) = cursor.split_at(4);
    *cursor = remainder;
    Some(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize)
}
