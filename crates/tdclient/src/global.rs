//! Port of `td/telegram/Global.{h,cpp}` (Track I, Global M.1): the shared
//! context every manager reaches through the C++ `G()` macro. The C++
//! Global is a context-installed singleton; the Rust one is an owned
//! struct inside the Td actor, handed to managers as `&Global` — the same
//! single-threaded access pattern without the thread-local.
//!
//! M.1 carries the load-bearing core: the `TdDb` home (`td_db()`), the
//! close flag, `is_test_dc`, and the server-time correlation
//! (`server_time_difference_` / `unix_time()`). The manager actor-id
//! setters (`set_user_manager`, ...) accumulate here as the managers land.

use tddb::binlog_key_value::BinlogKeyValue;
use tddb::td_db::TdDb;

/// `td::Global` — the M.1 subset.
pub struct Global {
    db: Option<TdDb>,
    /// `Global::close_flag_`.
    close_flag: bool,
    /// `Global::is_test_dc()` — read from the database parameters.
    is_test_dc: bool,
    /// `Global::server_time_difference_` — set by the ConfigManager /
    /// UpdatesManager when the server time is learned; 0 before that.
    server_time_difference: f64,
    /// `Global::server_time_difference_was_updated_`.
    server_time_difference_was_updated: bool,
}

impl Default for Global {
    fn default() -> Self {
        Self::new()
    }
}

impl Global {
    pub fn new() -> Global {
        Global {
            db: None,
            close_flag: false,
            is_test_dc: false,
            server_time_difference: 0.0,
            server_time_difference_was_updated: false,
        }
    }

    /// `Global::init` — take the opened database and the derived flags.
    pub fn init(&mut self, db: TdDb) {
        self.is_test_dc = db.parameters().is_test_dc;
        self.db = Some(db);
    }

    /// `Global::td_db()`.
    pub fn td_db(&mut self) -> &mut TdDb {
        self.db
            .as_mut()
            .expect("Global::td_db accessed before init or after close")
    }

    /// `Global::close_flag()`.
    pub fn close_flag(&self) -> bool {
        self.close_flag
    }

    /// `Global::set_close_flag()`.
    pub fn set_close_flag(&mut self) {
        self.close_flag = true;
    }

    /// `Global::is_test_dc()`.
    pub fn is_test_dc(&self) -> bool {
        self.is_test_dc
    }

    /// `Global::get_server_time_difference()`.
    pub fn get_server_time_difference(&self) -> f64 {
        self.server_time_difference
    }

    /// `Global::set_server_time_difference`.
    pub fn set_server_time_difference(&mut self, difference: f64) {
        self.server_time_difference = difference;
        self.server_time_difference_was_updated = true;
    }

    /// `Global::server_time_difference_was_updated()`.
    pub fn server_time_difference_was_updated(&self) -> bool {
        self.server_time_difference_was_updated
    }

    /// `Global::server_time()` — `Time::now() + difference` (the Rust
    /// `tdutils::time::now` is the same process-anchored monotonic clock).
    pub fn server_time(&self) -> f64 {
        tdutils::time::now() + self.server_time_difference
    }

    /// `Global::unix_time()` — the server-time-corrected unix timestamp.
    pub fn unix_time(&self) -> i32 {
        to_unix_time(self.server_time())
    }

    /// `Global::close_all` — drop the database (the manager-actor resets
    /// accumulate here as the managers land).
    pub fn close_all(&mut self, destroy_flag: bool) {
        if let Some(db) = &mut self.db {
            db.close_all(destroy_flag);
        }
        self.db = None;
    }

    /// The binlog pmc accessor (`G()->td_db()->get_binlog_pmc()`), the
    /// shape every manager's persistence goes through.
    pub fn binlog_pmc(&mut self) -> &mut BinlogKeyValue {
        self.td_db().binlog_pmc()
    }
}

/// `Global::to_unix_time` (Global.cpp:190-195) — the range-checked
/// truncation (`LOG_CHECK` panics on violation, like the C++).
fn to_unix_time(server_time: f64) -> i32 {
    assert!(
        (1.0..=2140000000.0).contains(&server_time),
        "server time out of range: {server_time}"
    );
    server_time as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_unix_time_is_the_checked_truncation() {
        assert_eq!(to_unix_time(1760000000.0), 1760000000);
        assert_eq!(to_unix_time(1760000000.75), 1760000000);
        assert_eq!(to_unix_time(1.0), 1);
        let result = std::panic::catch_unwind(|| to_unix_time(0.5));
        assert!(result.is_err(), "below the C++ LOG_CHECK range");
    }

    #[test]
    fn server_time_applies_the_difference() {
        let mut global = Global::new();
        assert!(!global.server_time_difference_was_updated());
        global.set_server_time_difference(12.5);
        assert!(global.server_time_difference_was_updated());
        let local = tdutils::time::now();
        let server = global.server_time();
        assert!((server - local - 12.5).abs() < 0.01);
        assert_eq!(global.get_server_time_difference(), 12.5);
    }
}
