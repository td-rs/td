//! Port of `td/telegram/Global.{h,cpp}` (Track I, Global/TimeZones): the
//! shared context every manager reaches through the C++ `G()` macro. The
//! C++ Global is a context-installed singleton; the Rust one is an owned
//! struct inside the Td actor, handed to managers as `&Global` — the same
//! single-threaded access pattern without the thread-local.
//!
//! Carried here: the `TdDb` home (`td_db()`), the close flag, `is_test_dc`,
//! and the full server-time-difference machinery — the persisted
//! `ServerTimeDiff` anchor with the `Global::init` clock repairs, the
//! gated `update_server_time_difference` save path and the corrected
//! `unix_time()` (M.2). The manager actor-id setters (`set_user_manager`,
//! ...) accumulate here as the managers land; the net_query_creator /
//! dispatcher and the location access hashes are the net step.

use tddb::binlog_key_value::BinlogKeyValue;
use tddb::td_db::TdDb;

use crate::option_manager::OptionManager;

/// The `server_time_difference` binlog pmc key (Global.cpp:96).
const SERVER_TIME_DIFFERENCE_KEY: &str = "server_time_difference";

/// The `ServerTimeDiff` struct (Global.cpp:70-90): the persisted
/// `server_time - Clocks::system` difference plus the wall clock it was
/// saved at. Serialized as the two raw `f64`s; the legacy 8-byte form (no
/// `system_time`) parses with `system_time == 0` (Global.cpp:84-88).
#[derive(Clone, Copy, Debug, PartialEq)]
struct ServerTimeDiff {
    diff: f64,
    system_time: f64,
}

impl ServerTimeDiff {
    fn store(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(16);
        out.extend_from_slice(&self.diff.to_le_bytes());
        out.extend_from_slice(&self.system_time.to_le_bytes());
        out
    }

    /// The `parse` of Global.cpp:80-89. Errors mirror the `TlParser`
    /// messages; `Global::init` aborts on them (`unserialize(...).ensure()`).
    fn parse(data: &[u8]) -> Result<ServerTimeDiff, &'static str> {
        if data.len() < 8 {
            return Err("Failed to read 8 bytes");
        }
        let diff = f64::from_le_bytes(data[0..8].try_into().unwrap());
        let system_time = if data.len() != 8 {
            if data.len() < 16 {
                return Err("Failed to read 8 bytes");
            }
            f64::from_le_bytes(data[8..16].try_into().unwrap())
        } else {
            0.0
        };
        if data.len() > 16 {
            return Err("Extra data in end of parsed string");
        }
        Ok(ServerTimeDiff { diff, system_time })
    }
}

/// `td::Global` — the M.1 subset with the M.2 server-time machinery.
pub struct Global {
    db: Option<TdDb>,
    /// `Global::close_flag_`.
    close_flag: bool,
    /// `Global::is_test_dc()` — read from the database parameters.
    is_test_dc: bool,
    /// `Global::server_time_difference_` — loaded (and repaired) from the
    /// binlog in `init`, updated by the net-driven steps.
    server_time_difference: f64,
    /// `Global::server_time_difference_was_updated_`.
    server_time_difference_was_updated: bool,
    /// `Global::dns_time_difference_` (Global.h:845).
    dns_time_difference: f64,
    /// `Global::dns_time_difference_was_updated_` (Global.h:846).
    dns_time_difference_was_updated: bool,
    /// `Global::system_time_saved_at_` — the 10-second re-anchor throttle.
    system_time_saved_at: f64,
    /// `Global::saved_diff_` — the raw persisted difference (diagnostics).
    saved_diff: f64,
    /// `Global::saved_system_time_` — the wall clock the blob was saved at.
    saved_system_time: f64,
}

impl Default for Global {
    fn default() -> Self {
        Self::new()
    }
}

impl Global {
    /// The C++ member defaults (Global.h:843-850).
    pub fn new() -> Global {
        Global {
            db: None,
            close_flag: false,
            is_test_dc: false,
            server_time_difference: 0.0,
            server_time_difference_was_updated: false,
            dns_time_difference: 0.0,
            dns_time_difference_was_updated: false,
            system_time_saved_at: -1e10,
            saved_diff: 0.0,
            saved_system_time: 0.0,
        }
    }

    /// `Global::init` (Global.cpp:92-143) — take the opened database and
    /// load the saved server time difference, repairing it against the wall
    /// clock: the persisted anchor is `Clocks::system`-based while the
    /// process clock (`Time::now`) is monotonic per incarnation, so each
    /// start re-derives `default_time_difference` and applies the
    /// backwards/forwards fixes.
    pub fn init(&mut self, db: TdDb) {
        self.is_test_dc = db.parameters().is_test_dc;
        self.db = Some(db);

        let saved_diff_str = self
            .binlog_pmc()
            .get_raw(SERVER_TIME_DIFFERENCE_KEY.as_bytes());
        self.load_server_time_difference(&saved_diff_str);
    }

    /// The body of `Global::init` from Global.cpp:99-140 — pure against the
    /// loaded blob, so the clock-fix branches are testable without a db.
    fn load_server_time_difference(&mut self, saved_diff_str: &[u8]) {
        let system_time = tdutils::time::system();
        let default_time_difference = system_time - tdutils::time::now();
        if saved_diff_str.is_empty() {
            self.server_time_difference = default_time_difference;
        } else {
            // `unserialize(saved_diff, saved_diff_str).ensure()` — malformed
            // saved data aborts the client.
            let saved_diff = match ServerTimeDiff::parse(saved_diff_str) {
                Ok(diff) => diff,
                Err(error) => panic!("Bad server time difference: {error}"),
            };

            if saved_diff.diff > 1e10
                || saved_diff.diff < -1e10
                || saved_diff.system_time > 1e10
                || saved_diff.system_time < 0.0
            {
                tracing::error!(
                    "Ignore definitely invalid saved server time difference {} {}",
                    saved_diff.diff,
                    saved_diff.system_time
                );
                self.server_time_difference = default_time_difference;
            } else {
                self.saved_diff = saved_diff.diff;
                self.saved_system_time = saved_diff.system_time;

                let mut diff = saved_diff.diff + default_time_difference;
                if saved_diff.system_time > system_time {
                    let time_backwards_fix = saved_diff.system_time - system_time;
                    if time_backwards_fix > 60.0 {
                        tracing::warn!(
                            "Fix system time which went backwards: {:.0} saved_system_time {} system_time {}",
                            time_backwards_fix,
                            saved_diff.system_time,
                            system_time
                        );
                    }
                    diff += time_backwards_fix;
                } else if saved_diff.system_time != 0.0 {
                    // if more than 1 year has passed, the session is logged out anyway
                    const MAX_TIME_FORWARD: f64 = 367.0 * 86400.0;
                    if saved_diff.system_time + MAX_TIME_FORWARD < system_time {
                        let time_forward_fix =
                            system_time - (saved_diff.system_time + MAX_TIME_FORWARD);
                        tracing::warn!(
                            "Fix system time which went forward: {:.0} saved_system_time {} system_time {}",
                            time_forward_fix,
                            saved_diff.system_time,
                            system_time
                        );
                        diff -= time_forward_fix;
                    }
                } else if saved_diff.diff >= 1_500_000_000.0 && system_time >= 1_500_000_000.0 {
                    // only for saved_diff.system_time == 0
                    diff = default_time_difference;
                }
                tracing::debug!("LOAD: server_time_difference {diff}");
                self.server_time_difference = diff;
            }
        }
        self.server_time_difference_was_updated = false;
        self.dns_time_difference = default_time_difference;
        self.dns_time_difference_was_updated = false;
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

    /// `Global::server_time_difference_was_updated()`.
    pub fn server_time_difference_was_updated(&self) -> bool {
        self.server_time_difference_was_updated
    }

    /// `Global::update_server_time_difference` (Global.cpp:197-205): the
    /// first server answer wins unless forced or a larger difference
    /// arrives; every accepted update is persisted and pushed to the
    /// OptionManager's `unix_time` notification (the C++ reaches it through
    /// the same hub; here it is a parameter).
    pub fn update_server_time_difference(
        &mut self,
        diff: f64,
        force: bool,
        option_manager: &mut OptionManager,
    ) {
        if force || !self.server_time_difference_was_updated || self.server_time_difference < diff {
            self.server_time_difference = diff;
            self.server_time_difference_was_updated = true;
            self.do_save_server_time_difference(option_manager);

            option_manager
                .on_update_server_time_difference(self.server_time_difference, self.unix_time());
        }
    }

    /// `Global::save_server_time` (Global.cpp:207-213): the periodic
    /// re-anchor, throttled to once per 10 seconds.
    pub fn save_server_time(&mut self, option_manager: &mut OptionManager) {
        let t = tdutils::time::now();
        if self.server_time_difference_was_updated && self.system_time_saved_at + 10.0 < t {
            self.system_time_saved_at = t;
            self.do_save_server_time_difference(option_manager);
        }
    }

    /// `Global::do_save_server_time_difference` (Global.cpp:215-230): store
    /// the difference re-anchored to `Clocks::system`, or erase it when the
    /// adjustment protection is disabled.
    fn do_save_server_time_difference(&mut self, option_manager: &OptionManager) {
        if option_manager.get_option_boolean("disable_time_adjustment_protection", false) {
            self.binlog_pmc().erase(SERVER_TIME_DIFFERENCE_KEY);
            return;
        }

        // diff = server_time - Time::now
        // fixed_diff = server_time - Clocks::system
        let system_time = tdutils::time::system();
        let fixed_diff = self.server_time_difference + tdutils::time::now() - system_time;

        let diff = ServerTimeDiff {
            diff: fixed_diff,
            system_time,
        };
        self.binlog_pmc()
            .set_raw(SERVER_TIME_DIFFERENCE_KEY.as_bytes(), &diff.store());
    }

    /// The raw persisted anchor for the reopen tests (the C++ keeps
    /// `saved_system_time_` private and re-derives it in init).
    #[cfg(test)]
    pub(crate) fn saved_system_time(&self) -> f64 {
        self.saved_system_time
    }

    /// `Global::update_dns_time_difference` (Global.cpp:232-235).
    pub fn update_dns_time_difference(&mut self, diff: f64) {
        self.dns_time_difference = diff;
        self.dns_time_difference_was_updated = true;
    }

    /// `Global::get_dns_time_difference` (Global.cpp:237-252).
    pub fn get_dns_time_difference(&self) -> f64 {
        let dns_flag = self.dns_time_difference_was_updated;
        let dns_diff = self.dns_time_difference;
        let server_flag = self.server_time_difference_was_updated;
        let server_diff = self.server_time_difference;
        if dns_flag != server_flag {
            return if dns_flag { dns_diff } else { server_diff };
        }
        if dns_flag {
            return dns_diff.max(server_diff);
        }
        if self.db.is_some() {
            return server_diff;
        }
        tdutils::time::system() - tdutils::time::now()
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
    use tddb::td_db::Parameters as DbParameters;

    /// A Global over a fresh throwaway TdDb; the directory is removed on
    /// drop.
    struct TempGlobal {
        global: Global,
        dir: String,
    }

    impl TempGlobal {
        fn new(name: &str) -> TempGlobal {
            let dir = std::env::temp_dir()
                .join(format!("td-rs-m21-global-{name}-{}", std::process::id()))
                .to_string_lossy()
                .into_owned();
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let opened = TdDb::open(DbParameters {
                database_directory: dir.clone(),
                ..Default::default()
            })
            .unwrap();
            let mut global = Global::new();
            global.init(opened.database);
            TempGlobal { global, dir }
        }
    }

    impl Drop for TempGlobal {
        fn drop(&mut self) {
            self.global.close_all(false);
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// `default_time_difference` recomputed the way `init` does; the clocks
    /// advance between calls, so compare with a tolerance.
    fn default_time_difference() -> f64 {
        tdutils::time::system() - tdutils::time::now()
    }

    fn assert_close(left: f64, right: f64) {
        assert!(
            (left - right).abs() < 2.0,
            "{left} and {right} must be close"
        );
    }

    fn diff_bytes(diff: f64, system_time: f64) -> Vec<u8> {
        ServerTimeDiff { diff, system_time }.store()
    }

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
        let mut temp = TempGlobal::new("apply");
        let global = &mut temp.global;
        assert!(!global.server_time_difference_was_updated());
        global.update_server_time_difference(12.5, false, &mut OptionManager::new(false));
        assert!(global.server_time_difference_was_updated());
        let local = tdutils::time::now();
        let server = global.server_time();
        assert!((server - local - 12.5).abs() < 0.01);
        assert_eq!(global.get_server_time_difference(), 12.5);
    }

    #[test]
    fn server_time_diff_stores_two_raw_f64s() {
        let bytes = diff_bytes(1.5, 1_700_000_000.0);
        assert_eq!(bytes.len(), 16);
        assert_eq!(&bytes[..8], &1.5f64.to_le_bytes());
        assert_eq!(&bytes[8..], &1_700_000_000.0f64.to_le_bytes());
        assert_eq!(
            ServerTimeDiff::parse(&bytes).unwrap(),
            ServerTimeDiff {
                diff: 1.5,
                system_time: 1_700_000_000.0
            }
        );
    }

    #[test]
    fn server_time_diff_parses_the_legacy_eight_byte_form() {
        let bytes = 12.25f64.to_le_bytes().to_vec();
        assert_eq!(
            ServerTimeDiff::parse(&bytes).unwrap(),
            ServerTimeDiff {
                diff: 12.25,
                system_time: 0.0
            }
        );
    }

    #[test]
    fn server_time_diff_rejects_short_and_extra_data() {
        assert!(ServerTimeDiff::parse(&[0u8; 7]).is_err());
        let mut bytes = diff_bytes(1.0, 2.0);
        bytes.push(0);
        assert!(ServerTimeDiff::parse(&bytes).is_err());
    }

    #[test]
    fn init_without_saved_data_uses_the_default_difference() {
        let mut global = Global::new();
        global.load_server_time_difference(&[]);
        assert_close(
            global.get_server_time_difference(),
            default_time_difference(),
        );
        assert!(!global.server_time_difference_was_updated());
        // dns_time_difference starts at the same default with its flag clear.
        assert_close(global.get_dns_time_difference(), default_time_difference());
    }

    #[test]
    fn init_keeps_a_recent_saved_difference_anchored_to_the_wall_clock() {
        let system_time = tdutils::time::system();
        let mut global = Global::new();
        global.load_server_time_difference(&diff_bytes(-3.0, system_time));
        // diff + default_time_difference, no fix applies (saved is in the past).
        assert_close(
            global.get_server_time_difference(),
            -3.0 + default_time_difference(),
        );
        assert_eq!(global.saved_system_time(), system_time);
    }

    #[test]
    fn init_fixes_a_system_time_that_went_backwards() {
        let system_time = tdutils::time::system();
        let mut global = Global::new();
        // Saved 120 seconds "in the future" relative to the current wall clock.
        global.load_server_time_difference(&diff_bytes(0.0, system_time + 120.0));
        assert_close(
            global.get_server_time_difference(),
            default_time_difference() + 120.0,
        );
    }

    #[test]
    fn init_fixes_a_system_time_that_went_forward() {
        let system_time = tdutils::time::system();
        let mut global = Global::new();
        // More than the 367-day MAX_TIME_FORWARD ahead: the excess is cut.
        global.load_server_time_difference(&diff_bytes(0.0, system_time - 367.0 * 86400.0 - 100.0));
        assert_close(
            global.get_server_time_difference(),
            default_time_difference() - 100.0,
        );
    }

    #[test]
    fn init_resets_an_ancient_flagless_difference() {
        let mut global = Global::new();
        // system_time == 0 (legacy form) with a huge diff and a modern wall
        // clock: the difference is reset to the default.
        global.load_server_time_difference(&2_000_000_000.0f64.to_le_bytes());
        assert_close(
            global.get_server_time_difference(),
            default_time_difference(),
        );
        assert_eq!(global.saved_system_time(), 0.0);
    }

    #[test]
    fn init_ignores_definitely_invalid_saved_data() {
        let mut global = Global::new();
        global.load_server_time_difference(&diff_bytes(2.0e10, 5.0));
        assert_close(
            global.get_server_time_difference(),
            default_time_difference(),
        );

        let mut global = Global::new();
        global.load_server_time_difference(&diff_bytes(1.0, -1.0));
        assert_close(
            global.get_server_time_difference(),
            default_time_difference(),
        );
    }

    #[test]
    fn update_server_time_difference_gates_and_persists() {
        let mut temp = TempGlobal::new("gates");
        let global = &mut temp.global;
        let mut option_manager = OptionManager::new(false);

        // First update wins without force.
        global.update_server_time_difference(42.0, false, &mut option_manager);
        assert_eq!(global.get_server_time_difference(), 42.0);
        assert!(global.server_time_difference_was_updated());
        assert!(global.binlog_pmc().get_raw(b"server_time_difference").len() == 16);

        // A smaller difference is ignored now.
        global.update_server_time_difference(10.0, false, &mut option_manager);
        assert_eq!(global.get_server_time_difference(), 42.0);
        // A larger one is taken; force takes anything.
        global.update_server_time_difference(43.0, false, &mut option_manager);
        assert_eq!(global.get_server_time_difference(), 43.0);
        global.update_server_time_difference(5.0, true, &mut option_manager);
        assert_eq!(global.get_server_time_difference(), 5.0);

        // The persisted anchor survives a reopen and repairs to the same
        // difference: diff(system) = server_diff + system() - now(), so the
        // init load returns diff + (system() - now()) = server_diff.
        global.close_all(false);

        let dir = temp.dir.clone();
        let reopened = TdDb::open(DbParameters {
            database_directory: dir,
            ..Default::default()
        })
        .unwrap();
        let mut global = Global::new();
        global.init(reopened.database);
        assert_close(global.get_server_time_difference(), 5.0);
    }

    #[test]
    fn do_save_erases_when_time_adjustment_protection_is_disabled() {
        let mut temp = TempGlobal::new("erase");
        let global = &mut temp.global;
        let mut option_manager = OptionManager::new(false);
        option_manager.set_option_boolean("disable_time_adjustment_protection", true);

        global.update_server_time_difference(11.0, false, &mut option_manager);
        assert_eq!(global.get_server_time_difference(), 11.0);
        assert_eq!(
            global.binlog_pmc().get_raw(b"server_time_difference"),
            Vec::<u8>::new()
        );
    }

    #[test]
    fn dns_time_difference_prefers_the_fresher_flag() {
        let mut temp = TempGlobal::new("dns");
        let global = &mut temp.global;
        global.update_server_time_difference(7.0, false, &mut OptionManager::new(false));

        // Server updated (flag set), dns not: the server difference wins
        // regardless of the dns value.
        assert_eq!(global.get_dns_time_difference(), 7.0);

        // Both updated: the maximum.
        global.update_dns_time_difference(9.0);
        assert_eq!(global.get_dns_time_difference(), 9.0);
        global.update_dns_time_difference(3.0);
        assert_eq!(global.get_dns_time_difference(), 7.0);

        // Fresh without td_db: the default wall-clock difference.
        let global = Global::new();
        assert_close(global.get_dns_time_difference(), default_time_difference());
    }
}
