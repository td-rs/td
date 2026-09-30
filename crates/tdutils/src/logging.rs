//! Port of `td/utils/logging.h` + `td/utils/logging.cpp`.
//!
//! tdlib's verbosity levels ([`PLAIN`]..[`NEVER`]) are kept exactly; each
//! accepted log line is emitted as a `tracing` event with the verbosity
//! mapped to a `tracing` level, and the default interface additionally
//! writes the classic colored tdlib format to stderr (C++ `DefaultLog`).

use std::cell::Cell;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicI32, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// `VERBOSITY_NAME(PLAIN)`: no header is added for this level.
pub const PLAIN: i32 = -1;
pub const FATAL: i32 = 0;
pub const ERROR: i32 = 1;
pub const WARNING: i32 = 2;
pub const INFO: i32 = 3;
pub const DEBUG: i32 = 4;
pub const NEVER: i32 = 1024;

/// `td::LogOptions`.
pub struct LogOptions {
    level: AtomicI32,
    fix_newlines: bool,
    add_info: bool,
}

impl LogOptions {
    const fn new(level: i32, fix_newlines: bool, add_info: bool) -> Self {
        Self {
            level: AtomicI32::new(level),
            fix_newlines,
            add_info,
        }
    }

    pub fn get_level(&self) -> i32 {
        self.level.load(Ordering::Relaxed)
    }

    pub fn set_level(&self, new_level: i32) -> i32 {
        self.level.swap(new_level, Ordering::Relaxed)
    }

    /// `td::LogOptions::plain()`: no header, no newline fixing.
    pub fn plain() -> &'static LogOptions {
        static PLAIN_OPTIONS: LogOptions = LogOptions::new(0, false, false);
        &PLAIN_OPTIONS
    }

    pub fn fix_newlines(&self) -> bool {
        self.fix_newlines
    }

    pub fn add_info(&self) -> bool {
        self.add_info
    }
}

impl Default for LogOptions {
    fn default() -> Self {
        Self::new(DEBUG + 1, true, true)
    }
}

/// `td::log_options`.
pub static LOG_OPTIONS: LogOptions = LogOptions::new(DEBUG + 1, true, true);

/// `td::set_verbosity_level`.
pub fn set_verbosity_level(level: i32) -> i32 {
    LOG_OPTIONS.set_level(level)
}

/// `td::get_verbosity_level`.
pub fn get_verbosity_level() -> i32 {
    LOG_OPTIONS.get_level()
}

/// `td::LogInterface`.
pub trait LogInterface: Send + Sync {
    fn do_append(&self, log_level: i32, slice: &str);

    /// `td::LogInterface::append`: forwards to `do_append`, then handles the
    /// `FATAL` level and the log message callback.
    fn append(&self, log_level: i32, slice: &str) {
        self.do_append(log_level, slice);
        if log_level == FATAL {
            process_fatal_error(slice);
        } else if log_level <= MAX_CALLBACK_VERBOSITY_LEVEL.load(Ordering::Relaxed)
            && let Some(callback) = *ON_LOG_MESSAGE_CALLBACK.lock().unwrap()
        {
            callback(log_level, slice);
        }
    }

    fn after_rotation(&self) {}

    fn get_file_paths(&self) -> Vec<String> {
        Vec::new()
    }
}

/// `td::OnLogMessageCallback`.
pub type OnLogMessageCallback = fn(i32, &str);

static MAX_CALLBACK_VERBOSITY_LEVEL: AtomicI32 = AtomicI32::new(-2);
static ON_LOG_MESSAGE_CALLBACK: Mutex<Option<OnLogMessageCallback>> = Mutex::new(None);

/// `td::set_log_message_callback`: a plain `fn` pointer like the C++
/// `OnLogMessageCallback` (TDLib uses `extern "C"` callbacks from hosts).
pub fn set_log_message_callback(max_verbosity_level: i32, callback: Option<OnLogMessageCallback>) {
    let max_level = match callback {
        Some(_) => max_verbosity_level,
        None => -2,
    };
    MAX_CALLBACK_VERBOSITY_LEVEL.store(max_level, Ordering::Relaxed);
    *ON_LOG_MESSAGE_CALLBACK.lock().unwrap() = callback;
}

/// `td::process_fatal_error`: forwards the message to the callback (if any
/// accepts level 0) and aborts.
pub fn process_fatal_error(message: &str) -> ! {
    if MAX_CALLBACK_VERBOSITY_LEVEL.load(Ordering::Relaxed) >= 0
        && let Some(callback) = *ON_LOG_MESSAGE_CALLBACK.lock().unwrap()
    {
        callback(0, message);
    }
    std::process::abort();
}

/// `td::FileLog` — the file sink with size-based rotation (the Rust port of
/// `td/utils/FileLog.cpp`). Interior mutability because `LogInterface::append`
/// takes `&self`; the C++ poll-time `fd_` becomes a `Mutex<Option<File>>`.
///
/// Divergence: the C++ `redirect_stderr` flag (duplicating the file
/// descriptor onto stderr) is not ported — mutating the host process's fds
/// from a library is not expressible safely in portable Rust; the default
/// interface's stderr/tracing emission is simply left alone.
#[derive(Debug)]
pub struct FileLog {
    path: String,
    rotate_threshold: AtomicI64,
    size: AtomicI64,
    file: Mutex<Option<std::fs::File>>,
}

impl FileLog {
    /// `td::FileLog::init`: open the path for appending (creating it when
    /// missing), remember the current size and the rotation threshold.
    pub fn init(path: String, rotate_threshold: i64) -> std::result::Result<Self, String> {
        if path.is_empty() {
            return Err("Log file path must be non-empty".to_string());
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|error| error.to_string())?;
        let size = file.metadata().map(|m| m.len() as i64).unwrap_or(0);
        Ok(Self {
            path,
            rotate_threshold: AtomicI64::new(rotate_threshold),
            size: AtomicI64::new(size),
            file: Mutex::new(Some(file)),
        })
    }

    /// `td::FileLog::set_rotate_threshold`.
    pub fn set_rotate_threshold(&self, rotate_threshold: i64) {
        self.rotate_threshold
            .store(rotate_threshold, Ordering::Relaxed);
    }

    /// `td::FileLog::get_rotate_threshold`.
    pub fn get_rotate_threshold(&self) -> i64 {
        self.rotate_threshold.load(Ordering::Relaxed)
    }

    /// `td::FileLog::lazy_rotate`: rotate only when the threshold has been
    /// exceeded (called once after `init` by the CLI, `cli.cpp`).
    pub fn lazy_rotate(&self) {
        if self.size.load(Ordering::Relaxed) > self.rotate_threshold.load(Ordering::Relaxed) {
            self.do_after_rotation();
        }
    }

    /// `td::FileLog::do_after_rotation`: close and reopen the file, then
    /// recover the size of what already existed on disk.
    fn do_after_rotation(&self) {
        *self.file.lock().unwrap() = None;
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            Ok(file) => {
                self.size.store(
                    file.metadata().map(|m| m.len() as i64).unwrap_or(0),
                    Ordering::Relaxed,
                );
                *self.file.lock().unwrap() = Some(file);
            }
            Err(error) => {
                process_fatal_error(&format!("{error} in {} at {}", file!(), line!()));
            }
        }
    }
}

impl LogInterface for FileLog {
    /// `td::FileLog::do_append`: rotate when the threshold has been exceeded
    /// (the C++ checks before writing, `size_ > rotate_threshold_`), write
    /// the slice, and warn about slow writes like the C++.
    fn do_append(&self, log_level: i32, slice: &str) {
        let start_time = std::time::Instant::now();
        if self.size.load(Ordering::Relaxed) > self.rotate_threshold.load(Ordering::Relaxed) {
            if let Err(error) = std::fs::rename(&self.path, format!("{}.old", self.path)) {
                process_fatal_error(&format!("{error} in {} at {}", file!(), line!()));
            }
            self.do_after_rotation();
        }
        let mut file = self.file.lock().unwrap();
        let file = match file.as_mut() {
            Some(file) => file,
            None => {
                process_fatal_error(&format!("File isn't opened in {} at {}", file!(), line!()));
            }
        };
        use std::io::Write as _;
        if let Err(error) = file.write_all(slice.as_bytes()) {
            process_fatal_error(&format!("{error} in {} at {}", file!(), line!()));
        }
        self.size.fetch_add(slice.len() as i64, Ordering::Relaxed);
        let total_time = start_time.elapsed().as_secs_f64();
        if total_time >= 0.1 && log_level >= 1 {
            let thread_id = get_thread_id();
            let pad = if (0..10).contains(&thread_id) {
                " "
            } else {
                ""
            };
            let warning = format!(
                "[ 2][t{pad}{thread_id}] !!! Previous logging took {total_time} seconds !!!\n"
            );
            let _ = file.write_all(warning.as_bytes());
        }
    }

    fn after_rotation(&self) {
        if !self.path.is_empty() {
            self.do_after_rotation();
        }
    }

    fn get_file_paths(&self) -> Vec<String> {
        vec![self.path.clone(), format!("{}.old", self.path)]
    }
}

/// The active file sink (`td::Log::log_file_path` state from Log.cpp), plus
/// the rotation threshold shared with `set_file_path` (default 10 MB). An
/// `Arc` behind a read lock — writers clone the `Arc` so that logging never
/// serializes on the global lock, and a rotation from another thread stays
/// visible.
static FILE_LOG: RwLock<Option<Arc<FileLog>>> = RwLock::new(None);
static MAX_LOG_FILE_SIZE: AtomicI64 = AtomicI64::new(10 << 20);

/// `td::Log::set_file_path`: switch the active sink to the given file (or
/// back to the default interface on an empty path). Returns whether the file
/// was opened successfully.
pub fn set_file_path(file_path: &str) -> bool {
    let mut file_log = FILE_LOG.write().unwrap();
    if file_path.is_empty() {
        *file_log = None;
        return true;
    }
    match FileLog::init(
        file_path.to_string(),
        MAX_LOG_FILE_SIZE.load(Ordering::Relaxed),
    ) {
        Ok(log) => {
            *file_log = Some(Arc::new(log));
            true
        }
        Err(_) => false,
    }
}

/// `Logging::set_current_stream(logStreamFile)`: install a file sink with an
/// explicit threshold. An empty path fails with `FileLog::init`'s error
/// ("Log file path must be non-empty") — the empty-path reset only happens
/// in `Log::set_file_path` above.
pub fn install_file_log(file_path: &str, rotate_threshold: i64) -> std::result::Result<(), String> {
    let log = FileLog::init(file_path.to_string(), rotate_threshold)?;
    *FILE_LOG.write().unwrap() = Some(Arc::new(log));
    Ok(())
}

/// The active file sink's `(path, rotate_threshold)` — the data behind
/// `Logging::get_current_stream()`'s `logStreamFile` answer.
pub fn file_log_info() -> Option<(String, i64)> {
    FILE_LOG
        .read()
        .unwrap()
        .as_ref()
        .map(|log| (log.path.clone(), log.get_rotate_threshold()))
}

/// `td::Log::set_max_file_size`: clamp to at least 1 and apply to the active
/// file sink.
pub fn set_max_file_size(max_file_size: i64) {
    let size = max_file_size.max(1);
    MAX_LOG_FILE_SIZE.store(size, Ordering::Relaxed);
    if let Some(file_log) = FILE_LOG.read().unwrap().as_ref() {
        file_log.set_rotate_threshold(size);
    }
}

thread_local! {
    /// `td::Logger::tag_` / `td::Logger::tag2_` — thread-local log context.
    static LOG_TAG: Cell<&'static str> = const { Cell::new("") };
    static LOG_TAG2: Cell<&'static str> = const { Cell::new("") };
    /// `td::port::get_thread_id` backing storage.
    static THREAD_ID: Cell<i32> = const { Cell::new(0) };
}

pub fn set_log_tag(tag: &'static str) {
    LOG_TAG.with(|cell| cell.set(tag));
}

/// The current `#` tag (the C++ `Log::tag_`): the actor-context instance
/// tag while an actor dispatch is running, empty on bare scheduler
/// machinery.
pub fn current_log_tag() -> &'static str {
    LOG_TAG.with(|cell| cell.get())
}

pub fn set_log_tag2(tag: &'static str) {
    LOG_TAG2.with(|cell| cell.set(tag));
}

/// `td::set_thread_id`.
pub fn set_thread_id(id: i32) {
    THREAD_ID.with(|cell| cell.set(id));
}

/// The C++ `detail::ThreadIdManager` counter — every thread that logs
/// registers itself with the next id, so the first (main) thread is 1 like
/// in TDLib.
static NEXT_THREAD_ID: AtomicI32 = AtomicI32::new(1);

/// `td::get_thread_id` — lazily registers the calling thread like the C++
/// `ThreadIdManager::register_thread`.
pub fn get_thread_id() -> i32 {
    THREAD_ID.with(|cell| {
        let id = cell.get();
        if id != 0 {
            return id;
        }
        let assigned = NEXT_THREAD_ID.fetch_add(1, Ordering::Relaxed);
        cell.set(assigned);
        assigned
    })
}

/// The named verbosity levels, one constant per C++
/// `int VERBOSITY_NAME(x) = VERBOSITY_NAME(DEBUG) + N;` runtime global.
/// Sites logged through them carry `[&x]` in the header (the `VLOG(x)`
/// comment), and the numeric level IS the tag identity, like in C++.
pub mod verbosity {
    use super::{DEBUG, INFO};

    /// `VERBOSITY_NAME(td_requests)` (Td.cpp:125).
    pub const TD_REQUESTS: i32 = INFO;
    /// `VERBOSITY_NAME(notifications)` (NotificationManager.cpp:71).
    pub const NOTIFICATIONS: i32 = INFO;
    /// `VERBOSITY_NAME(get_difference)` (UpdatesManager.cpp:124).
    pub const GET_DIFFERENCE: i32 = INFO;
    /// `VERBOSITY_NAME(dns_resolver)`.
    pub const DNS_RESOLVER: i32 = DEBUG;
    /// `VERBOSITY_NAME(file_loader)`.
    pub const FILE_LOADER: i32 = DEBUG;
    /// `VERBOSITY_NAME(proxy)`.
    pub const PROXY: i32 = DEBUG;
    /// `VERBOSITY_NAME(dc)` — DEBUG + 2.
    pub const DC: i32 = DEBUG + 2;
    /// `VERBOSITY_NAME(td_init)` — DEBUG + 3.
    pub const TD_INIT: i32 = DEBUG + 3;
    /// `VERBOSITY_NAME(mtproto)` — DEBUG + 7.
    pub const MTPROTO: i32 = DEBUG + 7;
    /// `VERBOSITY_NAME(binlog)` (tddb Binlog.cpp:171) — DEBUG + 8.
    pub const BINLOG: i32 = DEBUG + 8;
    /// `VERBOSITY_NAME(fd)` — DEBUG + 9.
    pub const FD: i32 = DEBUG + 9;
    /// `VERBOSITY_NAME(actor)` (tdactor Scheduler.cpp:36) — DEBUG + 10.
    pub const ACTOR: i32 = DEBUG + 10;
    /// `VERBOSITY_NAME(raw_mtproto)` — DEBUG + 10.
    pub const RAW_MTPROTO: i32 = DEBUG + 10;
    /// `VERBOSITY_NAME(sqlite)` — DEBUG + 10.
    pub const SQLITE: i32 = DEBUG + 10;
    /// `VERBOSITY_NAME(waiter)` — DEBUG + 10.
    pub const WAITER: i32 = DEBUG + 10;
}

/// Interns a dynamic log-tag string (e.g. an actor name) into `&'static
/// str` for the `Cell<&'static str>` tag slots — bounded in practice: the
/// tag set per process is the finite set of actor names.
pub fn intern_log_tag(tag: &str) -> &'static str {
    static INTERNED: Mutex<Option<std::collections::HashMap<String, &'static str>>> =
        Mutex::new(None);
    let mut guard = INTERNED.lock().unwrap();
    let map = guard.get_or_insert_with(std::collections::HashMap::new);
    if let Some(existing) = map.get(tag) {
        return existing;
    }
    let leaked: &'static str = Box::leak(tag.to_string().into_boxed_str());
    map.insert(tag.to_string(), leaked);
    leaked
}

/// The current `LOG_TAG2` value — for save/restore around actor dispatch.
pub fn current_log_tag2() -> &'static str {
    LOG_TAG2.with(Cell::get)
}

/// The C++ `format::as_hex_dump<4>` (format.h): a leading newline, a
/// partial first group of `size % 4` bytes, then 4-byte groups printed
/// byte-reversed (the little-endian `HexDumpSize<4, true>` display), a
/// newline every 16 groups and after the last one, a space between the
/// rest — exactly what `LOG << ... << format::as_hex_dump<4>(slice)`
/// appends.
pub fn hex_dump_4(data: &[u8]) -> String {
    fn group(bytes: &[u8]) -> String {
        bytes.iter().rev().map(|b| format!("{b:02x}")).collect()
    }
    let mut out = String::from("\n");
    let size = data.len();
    let first = size % 4;
    if first != 0 {
        out.push_str(&group(&data[..first]));
        out.push('\n');
    }
    let mut i = first;
    while i < size {
        out.push_str(&group(&data[i..i + 4]));
        if (i / 4) & 15 == 15 || i + 4 >= size {
            out.push('\n');
        } else {
            out.push(' ');
        }
        i += 4;
    }
    out
}

/// Maps a tdlib verbosity level to the `tracing` level used for the event
/// emitted alongside the stderr line.
pub fn verbosity_to_tracing_level(verbosity: i32) -> tracing::Level {
    match verbosity {
        _ if verbosity <= PLAIN => tracing::Level::TRACE,
        FATAL | ERROR => tracing::Level::ERROR,
        WARNING => tracing::Level::WARN,
        INFO => tracing::Level::INFO,
        _ => tracing::Level::DEBUG,
    }
}

/// Builds the tdlib log header: `[level][tN][unix.nanos][file:line][#tag]...\t`
/// — pure for testability; `Logger` fields are passed in explicitly.
#[allow(clippy::too_many_arguments)] // mirrors the C++ Logger ctor field-for-field
pub fn format_header(
    level: i32,
    thread_id: i32,
    time: SystemTime,
    file_name: &str,
    line_num: u32,
    tag: &str,
    tag2: &str,
    comment: &str,
) -> String {
    let mut out = String::new();
    // log level
    out.push('[');
    if level.unsigned_abs() < 10 {
        let _ = write!(out, " {level}");
    } else {
        let _ = write!(out, "{level}");
    }
    out.push(']');

    // thread identifier
    let _ = write!(out, "[t");
    if thread_id.unsigned_abs() < 10 {
        let _ = write!(out, " {thread_id}");
    } else {
        let _ = write!(out, "{thread_id}");
    }
    out.push(']');

    // timestamp
    let since_epoch = time
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    let unix_time = since_epoch as u32;
    let nanoseconds = ((since_epoch - since_epoch.floor()) * 1e9) as u32;
    let _ = write!(out, "[{unix_time}.{nanoseconds:09}]");

    // file : line (basename only)
    if !file_name.is_empty() {
        let base = file_name.rsplit(['/', '\\']).next().unwrap_or(file_name);
        let _ = write!(out, "[{base}:{line_num}]");
    }

    // context from tag_ / tag2_
    if !tag.is_empty() {
        let _ = write!(out, "[#{tag}]");
    }
    if !tag2.is_empty() {
        let _ = write!(out, "[!{tag2}]");
    }

    // comment (e.g. the condition in LOG_IF)
    if !comment.is_empty() {
        let _ = write!(out, "[&{comment}]");
    }

    out.push('\t');
    out
}

/// `td::Logger::~Logger` newline fixing: exactly one trailing newline.
pub fn fix_newlines(message: &str) -> String {
    let mut s = format!("{message}\n");
    while s.len() > 1 && s.as_bytes()[s.len() - 2] == b'\n' {
        s.pop();
    }
    s
}

/// The final message preparation of the C++ `Logger`: header + message, with
/// newline fixing applied when the options ask for it.
pub fn prepare_message(
    options: &LogOptions,
    level: i32,
    message: &str,
    file_name: &str,
    line_num: u32,
    comment: &str,
) -> String {
    let header = if level == PLAIN && std::ptr::eq(options, &LOG_OPTIONS) || !options.add_info() {
        String::new()
    } else {
        format_header(
            level,
            get_thread_id(),
            SystemTime::now(),
            file_name,
            line_num,
            LOG_TAG.with(Cell::get),
            LOG_TAG2.with(Cell::get),
            comment,
        )
    };
    let message = format!("{header}{message}");
    if options.fix_newlines() {
        fix_newlines(&message)
    } else {
        message
    }
}

/// `td::DefaultLog`: writes the tdlib format to stderr with the classic ANSI
/// colors and emits the same line as a `tracing` event.
pub struct DefaultLog;

/// `td::NullLog` — the `logStreamEmpty` sink: discards every message.
pub struct NullLog;

impl LogInterface for NullLog {
    fn do_append(&self, _log_level: i32, _slice: &str) {}
}

/// `td::null_log` / the `&null_log` sentinel of `Logging::get_current_stream`.
pub fn null_log_interface() -> &'static dyn LogInterface {
    static NULL_LOG: NullLog = NullLog;
    &NULL_LOG
}

impl DefaultLog {
    fn write_stderr(log_level: i32, slice: &str) {
        let (color, no_color): (&str, &str) = match log_level {
            FATAL | ERROR => ("\x1b[1;31m", "\x1b[0m"),
            WARNING => ("\x1b[1;33m", "\x1b[0m"),
            INFO => ("\x1b[1;36m", "\x1b[0m"),
            _ => ("", ""),
        };
        if !slice.is_empty() && slice.ends_with('\n') {
            eprintln!("{color}{}{no_color}", &slice[..slice.len() - 1]);
        } else {
            eprint!("{color}{slice}{no_color}");
        }
    }
}

impl LogInterface for DefaultLog {
    fn do_append(&self, log_level: i32, slice: &str) {
        Self::write_stderr(log_level, slice);
        // `tracing::event!` needs a constant callsite level, so dispatch on
        // the fixed level macros.
        match verbosity_to_tracing_level(log_level) {
            tracing::Level::ERROR => tracing::error!(target: "tdutils", "{}", slice),
            tracing::Level::WARN => tracing::warn!(target: "tdutils", "{}", slice),
            tracing::Level::INFO => tracing::info!(target: "tdutils", "{}", slice),
            tracing::Level::DEBUG => tracing::debug!(target: "tdutils", "{}", slice),
            tracing::Level::TRACE => tracing::trace!(target: "tdutils", "{}", slice),
        }
    }
}

/// `td::default_log_interface`.
pub static DEFAULT_LOG_INTERFACE: DefaultLog = DefaultLog;

/// `td::log_interface`: the active interface, initially the default (the C++
/// global starts out pointing at `default_log_interface` and is reassigned
/// freely). A fat pointer, so it is kept behind an `RwLock` rather than an
/// `AtomicPtr` (which would drop the vtable).
static LOG_INTERFACE: RwLock<&'static dyn LogInterface> = RwLock::new(&DEFAULT_LOG_INTERFACE);

pub fn default_log_interface() -> &'static dyn LogInterface {
    &DEFAULT_LOG_INTERFACE
}

/// What `set_log_interface` was last pointed at — the `Logging::get_current_stream`
/// answer needs this identity (`log_interface == default_log_interface` etc.);
/// comparing trait-object pointers is unreliable across codegen units
/// (duplicated vtables in debug builds).
static INTERFACE_TAG: AtomicI32 = AtomicI32::new(0);

/// 0 = the default interface, 1 = the null log, 2 = a custom interface.
pub fn current_interface_tag() -> i32 {
    INTERFACE_TAG.load(Ordering::Relaxed)
}

pub fn set_log_interface(interface: Option<&'static dyn LogInterface>) {
    let tag = match interface {
        None => 0,
        Some(interface) => {
            let data_ptr = interface as *const dyn LogInterface as *const ();
            let null_ptr = null_log_interface() as *const dyn LogInterface as *const ();
            if data_ptr == null_ptr { 1 } else { 2 }
        }
    };
    INTERFACE_TAG.store(tag, Ordering::Relaxed);
    *LOG_INTERFACE.write().unwrap() = interface.unwrap_or_else(default_log_interface);
}

/// Returns the active interface (the default when none is installed).
pub fn log_interface() -> &'static dyn LogInterface {
    *LOG_INTERFACE.read().unwrap()
}

/// `td::LogGuard` — a spinlock that serializes non-reentrant log sinks.
static LOG_GUARD: AtomicI32 = AtomicI32::new(0);

pub struct LogGuard;

impl LogGuard {
    pub fn acquire() -> Self {
        loop {
            if LOG_GUARD
                .compare_exchange(0, 1, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                return Self;
            }
            std::hint::spin_loop();
        }
    }
}

impl Drop for LogGuard {
    fn drop(&mut self) {
        debug_assert_eq!(LOG_GUARD.load(Ordering::Relaxed), 1);
        LOG_GUARD.store(0, Ordering::Relaxed);
    }
}

/// `td::has_log_guard`.
pub fn has_log_guard() -> bool {
    LOG_GUARD.load(Ordering::Relaxed) == 1
}

/// `td::ScopedDisableLog` — disables logging while any instance is alive.
pub struct ScopedDisableLog;

static SDL_MUTEX: Mutex<()> = Mutex::new(());
static SDL_CNT: AtomicI32 = AtomicI32::new(0);
static SDL_VERBOSITY: AtomicI32 = AtomicI32::new(0);

impl ScopedDisableLog {
    pub fn new() -> Self {
        let _guard = SDL_MUTEX.lock().unwrap();
        if SDL_CNT.fetch_add(1, Ordering::Relaxed) == 0 {
            SDL_VERBOSITY.store(set_verbosity_level(i32::MIN), Ordering::Relaxed);
        }
        Self
    }
}

impl Default for ScopedDisableLog {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for ScopedDisableLog {
    fn drop(&mut self) {
        let _guard = SDL_MUTEX.lock().unwrap();
        if SDL_CNT.fetch_sub(1, Ordering::Relaxed) == 1 {
            set_verbosity_level(SDL_VERBOSITY.load(Ordering::Relaxed));
        }
    }
}

/// Appends one message to the active log interface — the backend of the
/// [`log_message!`] macros. Filtering against the verbosity level happens in
/// the macros, like `LOG_IMPL_FULL` in C++.
#[doc(hidden)]
pub fn append_message(level: i32, message: &str, file_name: &str, line_num: u32, comment: &str) {
    let options = &LOG_OPTIONS;
    let message = prepare_message(options, level, message, file_name, line_num, comment);
    // The active sink: the file log when a path was set, the interface
    // otherwise — the C++ current-stream semantics (Log.cpp set_file_path).
    let file_log = FILE_LOG.read().unwrap().clone();
    if let Some(file_log) = file_log {
        file_log.append(level, &message);
    } else {
        log_interface().append(level, &message);
    }
}

/// The `LOG(level)` stream of C++ becomes a macro with a `format!`-style
/// message; the level is spelled out (`log_error!`, ...) and custom levels
/// use [`crate::vlog!`].
///
/// The `at "File.cpp", 123` form pins the header to the C++ source
/// location the site was ported from, so emitted lines are byte-shaped
/// like TDLib's (`[RawConnection.cpp:323]` instead of the Rust
/// `file!()`/`line!()`).
#[macro_export]
macro_rules! log_impl {
    ($level:expr, $comment:expr, at $file:literal, $line:expr, $($arg:tt)*) => {{
        let __log_level: i32 = $level;
        if __log_level <= $crate::logging::get_verbosity_level() {
            $crate::logging::append_message(
                __log_level,
                &::std::format!($($arg)*),
                $file,
                $line,
                $comment,
            );
        }
    }};
    ($level:expr, $comment:expr, $($arg:tt)*) => {{
        let __log_level: i32 = $level;
        if __log_level <= $crate::logging::get_verbosity_level() {
            $crate::logging::append_message(
                __log_level,
                &::std::format!($($arg)*),
                file!(),
                line!(),
                $comment,
            );
        }
    }};
}

/// `td::LOG(ERROR)`.
#[macro_export]
macro_rules! log_error {
    (at $file:literal, $line:expr, $($arg:tt)*) => {
        $crate::log_impl!($crate::logging::ERROR, "", at $file, $line, $($arg)*)
    };
    ($($arg:tt)*) => {
        $crate::log_impl!($crate::logging::ERROR, "", $($arg)*)
    };
}

/// `td::LOG(WARNING)`.
#[macro_export]
macro_rules! log_warning {
    (at $file:literal, $line:expr, $($arg:tt)*) => {
        $crate::log_impl!($crate::logging::WARNING, "", at $file, $line, $($arg)*)
    };
    ($($arg:tt)*) => {
        $crate::log_impl!($crate::logging::WARNING, "", $($arg)*)
    };
}

/// `td::LOG(INFO)`.
#[macro_export]
macro_rules! log_info {
    (at $file:literal, $line:expr, $($arg:tt)*) => {
        $crate::log_impl!($crate::logging::INFO, "", at $file, $line, $($arg)*)
    };
    ($($arg:tt)*) => {
        $crate::log_impl!($crate::logging::INFO, "", $($arg)*)
    };
}

/// `td::LOG(DEBUG)`.
#[macro_export]
macro_rules! log_debug {
    (at $file:literal, $line:expr, $($arg:tt)*) => {
        $crate::log_impl!($crate::logging::DEBUG, "", at $file, $line, $($arg)*)
    };
    ($($arg:tt)*) => {
        $crate::log_impl!($crate::logging::DEBUG, "", $($arg)*)
    };
}

/// `td::LOG(FATAL)` — appends and aborts the process.
#[macro_export]
macro_rules! log_fatal {
    (at $file:literal, $line:expr, $($arg:tt)*) => {{
        $crate::log_impl!($crate::logging::FATAL, "", at $file, $line, $($arg)*);
        // append() handles FATAL by aborting; this is unreachable but
        // `abort` is `!` while `append_message` returns normally, so make
        // the contract explicit for the optimizer.
        $crate::logging::process_fatal_error("")
    }};
    ($($arg:tt)*) => {{
        $crate::log_impl!($crate::logging::FATAL, "", $($arg)*);
        // append() handles FATAL by aborting; this is unreachable but
        // `abort` is `!` while `append_message` returns normally, so make
        // the contract explicit for the optimizer.
        $crate::logging::process_fatal_error("")
    }};
}

/// `td::VLOG(level)` — a custom verbosity level, logged at DEBUG filtering.
/// The 4-argument form pins the C++ source location AND passes the named
/// tag so the header carries `[&tag]` like the C++ `VLOG(tag)` comment.
#[macro_export]
macro_rules! vlog {
    ($level:expr, $tag:literal, at $file:literal, $line:expr, $($arg:tt)*) => {
        $crate::log_impl!($level, $tag, at $file, $line, $($arg)*)
    };
    ($level:expr, $($arg:tt)*) => {
        $crate::log_impl!($level, &::std::stringify!($level).to_string(), $($arg)*)
    };
}

/// `td::LOG_CHECK(condition)` — a fatal log unless the condition holds.
#[macro_export]
macro_rules! log_check {
    ($condition:expr) => {{
        if !$condition {
            $crate::log_impl!(
                $crate::logging::FATAL,
                ::std::stringify!($condition),
                ""
            );
            $crate::logging::process_fatal_error("")
        }
    }};
    ($condition:expr, $($arg:tt)*) => {{
        if !$condition {
            $crate::log_impl!(
                $crate::logging::FATAL,
                ::std::stringify!($condition),
                $($arg)*
            );
            $crate::logging::process_fatal_error("")
        }
    }};
}

/// `td::LOG_DCHECK` — like [`log_check!`] in debug builds, a no-op (without
/// evaluating the condition) in release builds, as with `NDEBUG` in C++.
#[macro_export]
macro_rules! log_dcheck {
    ($($arg:tt)*) => {
        #[cfg(debug_assertions)]
        $crate::log_check!($($arg)*);
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    /// The tests below mutate process-global logging state (verbosity level,
    /// active interface, callback), so they run under one lock — in parallel
    /// they would corrupt each other.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn lock_tests() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK.lock().unwrap()
    }

    struct CaptureLog {
        lines: Mutex<Vec<(i32, String)>>,
    }

    impl CaptureLog {
        fn new() -> Self {
            Self {
                lines: Mutex::new(Vec::new()),
            }
        }

        fn take(&self) -> Vec<(i32, String)> {
            std::mem::take(&mut *self.lines.lock().unwrap())
        }
    }

    impl LogInterface for CaptureLog {
        fn do_append(&self, log_level: i32, slice: &str) {
            self.lines
                .lock()
                .unwrap()
                .push((log_level, slice.to_string()));
        }
    }

    static CALLBACK_CALLS: AtomicUsize = AtomicUsize::new(0);
    static CALLBACK_LEVELS: Mutex<Vec<i32>> = Mutex::new(Vec::new());
    static CALLBACK_MESSAGES: Mutex<Vec<String>> = Mutex::new(Vec::new());

    fn capture_callback(log_level: i32, message: &str) {
        CALLBACK_CALLS.fetch_add(1, Ordering::Relaxed);
        CALLBACK_LEVELS.lock().unwrap().push(log_level);
        CALLBACK_MESSAGES.lock().unwrap().push(message.to_string());
    }

    #[test]
    fn verbosity_levels_match_cpp_constants() {
        let _lock = lock_tests();
        assert_eq!(PLAIN, -1);
        assert_eq!(FATAL, 0);
        assert_eq!(ERROR, 1);
        assert_eq!(WARNING, 2);
        assert_eq!(INFO, 3);
        assert_eq!(DEBUG, 4);
        assert_eq!(NEVER, 1024);
        assert_eq!(
            LogOptions::default().get_level(),
            DEBUG + 1,
            "default level"
        );
        assert_eq!(LogOptions::plain().get_level(), 0);
        assert!(!LogOptions::plain().add_info());
        assert!(!LogOptions::plain().fix_newlines());
    }

    #[test]
    fn log_interface_switch_and_default_restore() {
        let _lock = lock_tests();
        let capture = CaptureLog::new();
        set_log_interface(Some(unsafe {
            std::mem::transmute::<&dyn LogInterface, &'static dyn LogInterface>(&capture)
        }));
        set_verbosity_level(INFO);
        log_impl!(INFO, "", "info {}{}", 1, 2);
        log_impl!(DEBUG, "", "debug {}", 3);
        log_impl!(WARNING, "", "warning {}", 4);
        set_verbosity_level(DEBUG + 1);
        let lines = capture.take();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].0, INFO);
        assert!(lines[0].1.contains("info 12"));
        assert_eq!(lines[1].0, WARNING);
        set_log_interface(None);
        set_verbosity_level(DEBUG + 1);
        // Compare data pointers only: debug builds may duplicate vtables
        // across codegen units, making fat-pointer `ptr::eq` unreliable.
        let active = log_interface() as *const dyn LogInterface as *const ();
        let default =
            &DEFAULT_LOG_INTERFACE as &dyn LogInterface as *const dyn LogInterface as *const ();
        assert!(std::ptr::eq(active, default));
    }

    #[test]
    fn header_format_matches_cpp() {
        set_thread_id(7);
        set_log_tag("tag-a");
        set_log_tag2("tag-b");
        // 0.5 s is exactly representable in f64, so the fraction is precise.
        let time = UNIX_EPOCH + Duration::from_secs(1_735_000_000) + Duration::from_millis(500);
        let header = format_header(INFO, 7, time, "/a/b/file.cpp", 42, "tag-a", "tag-b", "cond");
        assert_eq!(
            header,
            "[ 3][t 7][1735000000.500000000][file.cpp:42][#tag-a][!tag-b][&cond]\t"
        );
        set_log_tag("");
        set_log_tag2("");
        set_thread_id(0);
    }

    #[test]
    fn header_pads_single_digits_and_omits_missing_parts() {
        let time = UNIX_EPOCH;
        let header = format_header(DEBUG, 12, time, "", 0, "", "", "");
        assert_eq!(header, "[ 4][t12][0.000000000]\t");
        let header = format_header(NEVER, 0, time, "", 0, "", "", "");
        assert_eq!(header, "[1024][t 0][0.000000000]\t");
    }

    #[test]
    fn fix_newlines_keeps_exactly_one_trailing_newline() {
        assert_eq!(fix_newlines("a"), "a\n");
        assert_eq!(fix_newlines("a\n"), "a\n");
        assert_eq!(fix_newlines("a\n\n\n"), "a\n");
        assert_eq!(fix_newlines("\n\n"), "\n");
        assert_eq!(fix_newlines(""), "\n");
    }

    #[test]
    fn plain_level_adds_no_header() {
        // PLAIN with the global options: no header, but newlines are fixed.
        let message = prepare_message(&LOG_OPTIONS, PLAIN, "data", "f.cpp", 1, "");
        assert_eq!(message, "data\n");
    }

    #[test]
    fn message_callback_receives_filtered_levels() {
        let _lock = lock_tests();
        CALLBACK_CALLS.store(0, Ordering::Relaxed);
        CALLBACK_LEVELS.lock().unwrap().clear();
        CALLBACK_MESSAGES.lock().unwrap().clear();
        set_log_message_callback(ERROR, Some(capture_callback));
        let capture = CaptureLog::new();
        capture.append(ERROR, "err");
        capture.append(WARNING, "warn"); // above max level -> not forwarded
        set_log_message_callback(0, None);
        assert_eq!(CALLBACK_CALLS.load(Ordering::Relaxed), 1);
        assert_eq!(*CALLBACK_LEVELS.lock().unwrap(), [ERROR]);
        assert_eq!(CALLBACK_MESSAGES.lock().unwrap()[0], "err");
    }

    #[test]
    fn scoped_disable_log_restores_verbosity() {
        let _lock = lock_tests();
        set_verbosity_level(INFO);
        {
            let _disabled = ScopedDisableLog::new();
            assert_eq!(get_verbosity_level(), i32::MIN);
            {
                let _nested = ScopedDisableLog::new();
                assert_eq!(get_verbosity_level(), i32::MIN);
            }
            assert_eq!(get_verbosity_level(), i32::MIN);
        }
        assert_eq!(get_verbosity_level(), INFO);
        set_verbosity_level(DEBUG + 1);
    }

    #[test]
    fn log_guard_is_exclusive() {
        let _lock = lock_tests();
        let guard = LogGuard::acquire();
        assert!(has_log_guard());
        drop(guard);
        assert!(!has_log_guard());
    }

    #[test]
    fn tracing_level_mapping() {
        use tracing::Level;
        assert_eq!(verbosity_to_tracing_level(PLAIN), Level::TRACE);
        assert_eq!(verbosity_to_tracing_level(FATAL), Level::ERROR);
        assert_eq!(verbosity_to_tracing_level(ERROR), Level::ERROR);
        assert_eq!(verbosity_to_tracing_level(WARNING), Level::WARN);
        assert_eq!(verbosity_to_tracing_level(INFO), Level::INFO);
        assert_eq!(verbosity_to_tracing_level(DEBUG), Level::DEBUG);
        assert_eq!(verbosity_to_tracing_level(100), Level::DEBUG);
    }

    #[test]
    fn default_interface_is_used_when_none_installed() {
        let _lock = lock_tests();
        set_log_interface(None);
        // Compare data pointers only (see the note in the test above).
        let active = log_interface() as *const dyn LogInterface as *const ();
        let default =
            &DEFAULT_LOG_INTERFACE as &dyn LogInterface as *const dyn LogInterface as *const ();
        assert!(std::ptr::eq(active, default));
    }

    /// A fresh per-test directory for the file-log tests (parallel-safe even
    /// without the test lock).
    fn temp_log_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tdutils-filelog-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn file_log_init_rejects_empty_path() {
        let _lock = lock_tests();
        assert_eq!(
            FileLog::init(String::new(), 100).unwrap_err(),
            "Log file path must be non-empty"
        );
    }

    #[test]
    fn file_log_rotates_when_threshold_exceeded() {
        let _lock = lock_tests();
        let dir = temp_log_dir();
        let path = dir.join("log.txt");
        let log = FileLog::init(path.to_string_lossy().to_string(), 8).unwrap();
        assert_eq!(log.get_rotate_threshold(), 8);
        log.append(INFO, "hello world"); // 11 bytes > 8 -> the next append rotates
        log.append(INFO, "next line");
        let body = std::fs::read_to_string(&path).unwrap();
        assert_eq!(body, "next line");
        let old = std::fs::read_to_string(dir.join("log.txt.old")).unwrap();
        assert_eq!(old, "hello world");
        let paths = log.get_file_paths();
        assert_eq!(paths.len(), 2);
        assert!(paths[0].ends_with("log.txt"));
        assert!(paths[1].ends_with("log.txt.old"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn set_file_path_switches_the_active_sink() {
        let _lock = lock_tests();
        let dir = temp_log_dir();
        let path = dir.join("global.txt");
        let path_str = path.to_string_lossy().to_string();
        set_verbosity_level(INFO);
        assert!(set_file_path(&path_str));
        log_impl!(INFO, "", "to the file");
        assert!(set_file_path("")); // back to the default interface
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(
            body.contains("to the file"),
            "file log must receive the line"
        );
        set_verbosity_level(DEBUG + 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn set_max_file_size_clamps_and_updates_the_sink() {
        let _lock = lock_tests();
        let dir = temp_log_dir();
        let path = dir.join("sized.txt");
        assert!(set_file_path(path.to_string_lossy().as_ref()));
        set_max_file_size(0); // clamped to 1
        set_max_file_size(1 << 20);
        assert!(set_file_path(""));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
