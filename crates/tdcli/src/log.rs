//! M.14.1 — the CLI logging setup (`cli.cpp` `td::main`'s log block):
//! `CliLog`, the process-wide `combined_log`, and the verbosity wiring.
//!
//! C++ shape: `CliLog` (a passthrough to `default_log_interface` wrapped in
//! the readline deactivate/reactivate dance) → `combined_log.set_first`,
//! `log_interface = &combined_log`, first verbosity = INFO, and (by default)
//! a `TsLog`-wrapped `FileLog("tg_cli.log", 1000 << 20)` as the second sink
//! at DEBUG. The readline prompt redraw is not ported (no readline).

use std::sync::OnceLock;
use tdutils::combined_log::{CombinedLog, TsLog};
use tdutils::logging::{
    DEBUG, DEFAULT_LOG_INTERFACE, FileLog, INFO, LogInterface, set_log_interface,
    set_verbosity_level,
};

/// `class CliLog final : public LogInterface` (cli.cpp:214).
pub struct CliLog;

impl LogInterface for CliLog {
    fn do_append(&self, log_level: i32, slice: &str) {
        DEFAULT_LOG_INTERFACE.do_append(log_level, slice);
    }
}

/// The `CliLog cli_log` stack object — stateless, so a static.
pub static CLI_LOG: CliLog = CliLog;

/// `static CombinedLog combined_log`.
pub static COMBINED_LOG: CombinedLog = CombinedLog::new();

static FILE_LOG: OnceLock<FileLog> = OnceLock::new();
static TS_LOG: OnceLock<TsLog> = OnceLock::new();

/// The `td::main` log setup, ported in order. `-v`/`-l` argument parsing
/// lands in M.14.6 — the defaults match the C++ defaults.
pub fn setup() {
    COMBINED_LOG.set_first(&CLI_LOG);
    set_log_interface(Some(&COMBINED_LOG));

    let new_verbosity_level = INFO;
    // SET_VERBOSITY_LEVEL(td::max(new_verbosity_level, VERBOSITY_NAME(DEBUG)))
    set_verbosity_level(new_verbosity_level.max(DEBUG));
    COMBINED_LOG.set_first_verbosity_level(new_verbosity_level);

    // C++: `if (combined_log.get_first() == &cli_log)` — always true at this
    // point (nothing else sets it), so the branch is unconditional.
    let file_log = FILE_LOG.get_or_init(|| {
        FileLog::init("tg_cli.log".to_string(), 1000 << 20)
            .unwrap_or_else(|error| tdutils::logging::process_fatal_error(&error))
    });
    file_log.lazy_rotate();
    let ts_log = TS_LOG.get_or_init(|| TsLog::new(file_log as &'static dyn LogInterface));
    COMBINED_LOG.set_second(ts_log as &'static dyn LogInterface);
    COMBINED_LOG.set_second_verbosity_level(DEBUG);
}
