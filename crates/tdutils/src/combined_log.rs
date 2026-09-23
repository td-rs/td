//! `td/utils/CombinedLog.h` and `td/utils/TsLog.h` — the two-sink logger with
//! per-sink verbosity thresholds and the thread-safe delegating wrapper used
//! by the CLI (`cli.cpp`).
//!
//! The C++ `CombinedLog` holds raw `LogInterface *` into stack objects; the
//! Rust port requires `&'static dyn LogInterface` — the CLI installs statics
//! (its `CliLog`, `FileLog`, `TsLog` are stateless delegates).

use crate::logging::{FATAL, LogInterface};
use std::sync::Mutex;
use std::sync::atomic::{AtomicI32, Ordering};

/// `td::CombinedLog`: forwards a message to each sink whose verbosity
/// threshold the message's level does not exceed.
pub struct CombinedLog {
    first: Mutex<Option<&'static dyn LogInterface>>,
    first_verbosity_level: AtomicI32,
    second: Mutex<Option<&'static dyn LogInterface>>,
    second_verbosity_level: AtomicI32,
}

impl Default for CombinedLog {
    fn default() -> Self {
        Self::new()
    }
}

impl CombinedLog {
    pub const fn new() -> Self {
        Self {
            first: Mutex::new(None),
            first_verbosity_level: AtomicI32::new(FATAL),
            second: Mutex::new(None),
            second_verbosity_level: AtomicI32::new(FATAL),
        }
    }

    /// `td::CombinedLog::set_first`.
    pub fn set_first(&self, first: &'static dyn LogInterface) {
        *self.first.lock().unwrap() = Some(first);
    }

    /// `td::CombinedLog::set_second`.
    pub fn set_second(&self, second: &'static dyn LogInterface) {
        *self.second.lock().unwrap() = Some(second);
    }

    /// `td::CombinedLog::set_first_verbosity_level`.
    pub fn set_first_verbosity_level(&self, new_verbosity_level: i32) {
        self.first_verbosity_level
            .store(new_verbosity_level, Ordering::Relaxed);
    }

    /// `td::CombinedLog::set_second_verbosity_level`.
    pub fn set_second_verbosity_level(&self, new_verbosity_level: i32) {
        self.second_verbosity_level
            .store(new_verbosity_level, Ordering::Relaxed);
    }

    /// `td::CombinedLog::get_first`.
    pub fn get_first(&self) -> Option<&'static dyn LogInterface> {
        *self.first.lock().unwrap()
    }

    /// `td::CombinedLog::get_second`.
    pub fn get_second(&self) -> Option<&'static dyn LogInterface> {
        *self.second.lock().unwrap()
    }

    /// `td::CombinedLog::get_first_verbosity_level`.
    pub fn get_first_verbosity_level(&self) -> i32 {
        self.first_verbosity_level.load(Ordering::Relaxed)
    }

    /// `td::CombinedLog::get_second_verbosity_level`.
    pub fn get_second_verbosity_level(&self) -> i32 {
        self.second_verbosity_level.load(Ordering::Relaxed)
    }
}

impl LogInterface for CombinedLog {
    /// `td::CombinedLog::do_append`: forwards to each sink below its
    /// threshold (the C++ calls `do_append`, not `append` — no duplicated
    /// fatal processing).
    fn do_append(&self, log_level: i32, slice: &str) {
        if let Some(first) = *self.first.lock().unwrap()
            && log_level <= self.first_verbosity_level.load(Ordering::Relaxed)
        {
            first.do_append(log_level, slice);
        }
        if let Some(second) = *self.second.lock().unwrap()
            && log_level <= self.second_verbosity_level.load(Ordering::Relaxed)
        {
            second.do_append(log_level, slice);
        }
    }

    fn after_rotation(&self) {
        if let Some(first) = *self.first.lock().unwrap() {
            first.after_rotation();
        }
        if let Some(second) = *self.second.lock().unwrap() {
            second.after_rotation();
        }
    }

    fn get_file_paths(&self) -> Vec<String> {
        let mut result = Vec::new();
        if let Some(first) = *self.first.lock().unwrap() {
            result.extend(first.get_file_paths());
        }
        if let Some(second) = *self.second.lock().unwrap() {
            result.extend(second.get_file_paths());
        }
        result
    }
}

/// `td::TsLog`: the spinlock-serialized delegating wrapper (the "ts" is
/// thread-safety — the lock, not timestamps). The Rust `LogGuard` in
/// `logging.rs` already serializes non-reentrant sinks, so the delegate is
/// a plain pass-through here.
pub struct TsLog {
    log: Mutex<Option<&'static dyn LogInterface>>,
}

impl TsLog {
    pub fn new(log: &'static dyn LogInterface) -> Self {
        Self {
            log: Mutex::new(Some(log)),
        }
    }

    /// `td::TsLog::init`.
    pub fn init(&self, log: &'static dyn LogInterface) {
        *self.log.lock().unwrap() = Some(log);
    }
}

impl Default for TsLog {
    fn default() -> Self {
        Self {
            log: Mutex::new(None),
        }
    }
}

impl LogInterface for TsLog {
    fn do_append(&self, log_level: i32, slice: &str) {
        if let Some(log) = *self.log.lock().unwrap() {
            log.do_append(log_level, slice);
        }
    }

    fn after_rotation(&self) {
        if let Some(log) = *self.log.lock().unwrap() {
            log.after_rotation();
        }
    }

    fn get_file_paths(&self) -> Vec<String> {
        if let Some(log) = *self.log.lock().unwrap() {
            log.get_file_paths()
        } else {
            Vec::new()
        }
    }
}
