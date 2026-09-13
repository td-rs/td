//! Port of the `td/telegram/net` dispatch core — `DcId.h`, `NetQuery.h`
//! (+ `NetQuery.cpp`), `NetActor.{h,cpp}`, `NetQueryDispatcher.{h,cpp}`
//! and the `NetQueryDelayer` delay policy — as far as Track F reaches.
//!
//! `DcId` and the [`NetQuery`] state machine (Empty/Query/Ok/Error, the
//! Resend/Canceled/ResendInvokeAfter error codes, resend counters) are
//! verbatim; [`NetQueryDispatcher::dispatch`] keeps the exact C++ routing
//! order — stop flag, error-policy classification (303 migrate fix,
//! `Resend`, `FROZEN_METHOD_INVALID`, the delayer classes), the
//! `dispatch_ttl` exhaustion, main-DC resolution, DC-init gating and the
//! per-type session sink — with the delay policy of the C++
//! `NetQueryDelayer` (FLOOD_WAIT_/SLOWMODE_WAIT_/2FA_CONFIRM_WAIT_/
//! TAKEOUT_INIT_DELAY_/FLOOD_PREMIUM_WAIT_, clamped to 14 days, and the
//! `WORKER_BUSY_TOO_LONG_RETRY` 1-second resend).
//!
//! Documented divergences (the rest of the C++ layer belongs to Track G):
//! * The `SessionMultiProxy` per-(DC, type) actors, `DcAuthManager`,
//!   `NetQueryVerifier`, `MultiSequenceDispatcher` and `PublicRsaKeyWatchdog`
//!   are not ported; [`SessionSink`] is the plug-in point they implement.
//! * The C++ `wait_dc_init` requests DC initialization and waits; the
//!   slice only reports readiness (`mark_dc_valid`/`mark_dc_inited`) and
//!   fails with the C++ "No such dc" error otherwise.
//! * Completion without a per-query callback goes to the [`TdSink`]
//!   stand-in for C++ `Td::on_result`.
//! * `NetQueryDebug` logging and the ObjectPool-backed `NetQueryPtr` are
//!   not ported: queries are plain owned values.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};

use parking_lot::Mutex;

use tdutils::status::Status;

/// `DcId` — verbatim port.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DcId {
    dc_id: i32,
    is_external: bool,
}

impl DcId {
    pub const MAX_RAW_DC_ID: i32 = 1000;
    const EMPTY: i32 = 0;
    const MAIN_DC: i32 = -1;
    const INVALID: i32 = -2;

    pub fn is_valid_raw(dc_id: i32) -> bool {
        (1..=Self::MAX_RAW_DC_ID).contains(&dc_id)
    }

    pub fn main() -> Self {
        Self {
            dc_id: Self::MAIN_DC,
            is_external: false,
        }
    }

    pub fn invalid() -> Self {
        Self {
            dc_id: Self::INVALID,
            is_external: false,
        }
    }

    pub fn internal(id: i32) -> Self {
        assert!(Self::is_valid_raw(id), "invalid internal DC id {id}");
        Self {
            dc_id: id,
            is_external: false,
        }
    }

    pub fn external(id: i32) -> Self {
        assert!(Self::is_valid_raw(id), "invalid external DC id {id}");
        Self {
            dc_id: id,
            is_external: true,
        }
    }

    pub fn empty() -> Self {
        Self {
            dc_id: Self::EMPTY,
            is_external: false,
        }
    }

    pub fn from_value(value: i32) -> Self {
        Self {
            dc_id: value,
            is_external: false,
        }
    }

    pub fn create(dc_id_value: i32) -> Self {
        if Self::is_valid_raw(dc_id_value) {
            Self::internal(dc_id_value)
        } else {
            Self::invalid()
        }
    }

    pub fn is_empty(&self) -> bool {
        !self.is_valid()
    }

    pub fn is_main(&self) -> bool {
        self.dc_id == Self::MAIN_DC
    }

    pub fn is_exact(&self) -> bool {
        self.dc_id > 0
    }

    pub fn is_valid(&self) -> bool {
        self.is_exact() || self.is_main()
    }

    pub fn is_internal(&self) -> bool {
        !self.is_external
    }

    pub fn is_external(&self) -> bool {
        self.is_external
    }

    pub fn get_raw_id(&self) -> i32 {
        assert!(self.is_exact(), "raw id of a non-exact DC");
        self.dc_id
    }

    pub fn get_value(&self) -> i32 {
        self.dc_id
    }
}

/// `NetQuery::Type`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetQueryType {
    Common,
    Upload,
    Download,
    DownloadSmall,
}

/// `NetQuery::AuthFlag`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthFlag {
    Off,
    On,
}

/// `NetQuery::GzipFlag`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GzipFlag {
    Off,
    On,
}

/// `NetQuery::Error` — the reserved error codes.
pub const RESEND_ERROR_CODE: i32 = 202;
pub const CANCELED_ERROR_CODE: i32 = 203;
pub const RESEND_INVOKE_AFTER_ERROR_CODE: i32 = 204;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Empty,
    Query,
    Ok,
    Error,
}

/// `td::NetQuery` — the request envelope with its per-query state.
pub struct NetQuery {
    id: u64,
    dc_id: DcId,
    query_type: NetQueryType,
    auth_flag: AuthFlag,
    gzip_flag: GzipFlag,
    tl_constructor: i32,
    state: State,
    query: Vec<u8>,
    answer: Vec<u8>,
    error: Option<Status>,
    dispatch_ttl: u32,
    resend_count: u32,
    callback: Option<Box<dyn NetQueryCallback>>,
}

impl NetQuery {
    /// `NetQuery::make_constructor`-style creation: `query` is the already
    /// serialized TL body, `dc_id` may be `DcId::main()`.
    #[allow(clippy::too_many_arguments)] // the C++ factory has the same shape
    pub fn new(
        id: u64,
        dc_id: DcId,
        query_type: NetQueryType,
        auth_flag: AuthFlag,
        gzip_flag: GzipFlag,
        tl_constructor: i32,
        query: Vec<u8>,
        dispatch_ttl: u32,
    ) -> Self {
        Self {
            id,
            dc_id,
            query_type,
            auth_flag,
            gzip_flag,
            tl_constructor,
            state: State::Query,
            query,
            answer: Vec::new(),
            error: None,
            dispatch_ttl,
            resend_count: 0,
            callback: None,
        }
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn dc_id(&self) -> DcId {
        self.dc_id
    }

    pub fn query_type(&self) -> NetQueryType {
        self.query_type
    }

    pub fn auth_flag(&self) -> AuthFlag {
        self.auth_flag
    }

    pub fn gzip_flag(&self) -> GzipFlag {
        self.gzip_flag
    }

    pub fn tl_constructor(&self) -> i32 {
        self.tl_constructor
    }

    pub fn query(&self) -> &[u8] {
        &self.query
    }

    pub fn dispatch_ttl(&self) -> u32 {
        self.dispatch_ttl
    }

    pub fn resend_count(&self) -> u32 {
        self.resend_count
    }

    pub fn ok(&self) -> &[u8] {
        assert!(self.state == State::Ok, "ok() on a non-Ok query");
        &self.answer
    }

    pub fn error(&self) -> &Status {
        assert!(self.state == State::Error, "error() on a non-Error query");
        self.error.as_ref().expect("Error state without a status")
    }

    pub fn move_as_ok(&mut self) -> Vec<u8> {
        let answer = std::mem::take(&mut self.answer);
        self.clear();
        answer
    }

    pub fn move_as_error(&mut self) -> Status {
        let error = self
            .error
            .take()
            .expect("move_as_error on a non-Error query");
        self.clear();
        error
    }

    pub fn set_ok(&mut self, answer: Vec<u8>) {
        self.state = State::Ok;
        self.answer = answer;
        self.error = None;
    }

    pub fn set_error(&mut self, status: Status) {
        assert!(status.is_error(), "set_error with an OK status");
        self.state = State::Error;
        self.error = Some(status);
    }

    pub fn set_error_resend(&mut self) {
        self.set_error(Status::error_code::<RESEND_ERROR_CODE>());
    }

    pub fn set_error_canceled(&mut self) {
        self.set_error(Status::error_code::<CANCELED_ERROR_CODE>());
    }

    pub fn set_error_resend_invoke_after(&mut self) {
        self.set_error(Status::error_code::<RESEND_INVOKE_AFTER_ERROR_CODE>());
    }

    pub fn is_ready(&self) -> bool {
        self.state != State::Query
    }

    pub fn is_ok(&self) -> bool {
        self.state == State::Ok
    }

    pub fn is_error(&self) -> bool {
        self.state == State::Error
    }

    /// `NetQuery::resend(new_dc_id)` — re-arms the query toward
    /// `new_dc_id`.
    pub fn resend(&mut self, new_dc_id: DcId) {
        self.resend_count += 1;
        self.dc_id = new_dc_id;
        self.error = None;
        self.state = State::Query;
    }

    pub fn set_callback(&mut self, callback: Box<dyn NetQueryCallback>) {
        self.callback = Some(callback);
    }

    /// `move_callback`.
    pub fn move_callback(&mut self) -> Option<Box<dyn NetQueryCallback>> {
        self.callback.take()
    }

    fn clear(&mut self) {
        self.state = State::Empty;
        self.answer = Vec::new();
        self.error = None;
    }
}

/// `td::NetQueryCallback` — the receiver of completed queries.
pub trait NetQueryCallback: Send {
    fn on_result(&mut self, query: NetQuery);

    /// `on_result_resendable` — the handler may re-send the query through
    /// `resend` instead of consuming it. The default never re-sends.
    fn on_result_resendable(&mut self, query: NetQuery, resend: ResendFn) {
        let _ = resend;
        self.on_result(query);
    }
}

/// The `Promise<NetQueryPtr>` of `on_result_resendable`.
pub type ResendFn<'a> = &'a mut dyn FnMut(NetQuery);

/// `td::NetActor` — the base "component that sends net queries": its
/// `on_result` routes to [`NetActor::on_result_packet`] /
/// [`NetActor::on_error`] and notifies [`NetActor::on_result_finish`].
pub trait NetActor: Send {
    fn on_result_packet(&mut self, _packet: Vec<u8>) {}

    fn on_error(&mut self, _error: Status) {}

    fn on_result_finish(&mut self) {}

    /// `NetActor::on_result` — the result router.
    fn on_result(&mut self, mut query: NetQuery) {
        if query.is_error() {
            let error = query.move_as_error();
            self.on_error(error);
        } else {
            let packet = query.move_as_ok();
            self.on_result_packet(packet);
            self.on_result_finish();
        }
    }
}

/// The `SessionMultiProxy` plug-in point: route a ready query to its
/// destination DC session. Track G replaces the test implementations.
pub trait SessionSink: Send {
    fn send(&mut self, dc_id: DcId, query_type: NetQueryType, query: NetQuery);
}

/// The `Td::on_result` plug-in point for callback-less queries.
pub trait TdSink: Send {
    fn on_result(&mut self, query: NetQuery);
}

/// One delayed query of the `NetQueryDelayer` stand-in.
pub struct DelayedQuery {
    /// Unix time when the query becomes sendable again.
    pub ready_at: f64,
    /// The parsed delay (the clamped `FLOOD_WAIT_x` value).
    pub delay: f64,
    pub query: NetQuery,
}

/// `td::NetQueryDispatcher` — the Track F slice.
pub struct NetQueryDispatcher {
    stop_flag: AtomicBool,
    main_dc_id: AtomicI32,
    dcs: Mutex<HashMap<i32, DcState>>,
    session_sink: Mutex<Box<dyn SessionSink>>,
    td_sink: Mutex<Box<dyn TdSink>>,
    delayer: Mutex<Vec<DelayedQuery>>,
    query_id: AtomicU64,
}

#[derive(Default)]
struct DcState {
    is_valid: bool,
    is_inited: bool,
}

impl NetQueryDispatcher {
    pub fn new(session_sink: Box<dyn SessionSink>, td_sink: Box<dyn TdSink>) -> Self {
        Self {
            stop_flag: AtomicBool::new(false),
            main_dc_id: AtomicI32::new(1),
            dcs: Mutex::new(HashMap::new()),
            session_sink: Mutex::new(session_sink),
            td_sink: Mutex::new(td_sink),
            delayer: Mutex::new(Vec::new()),
            query_id: AtomicU64::new(0),
        }
    }

    /// `G()->request_connection_token`-style id generation for tests and
    /// callers that do not have `MtprotoClient` yet (Track G owns the real
    /// id allocation inside the MTProto layer).
    pub fn next_query_id(&self) -> u64 {
        self.query_id.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub fn get_main_dc_id(&self) -> DcId {
        DcId::internal(self.main_dc_id.load(Ordering::Relaxed))
    }

    pub fn set_main_dc_id(&self, new_main_dc_id: i32) {
        assert!(
            DcId::is_valid_raw(new_main_dc_id),
            "invalid main DC id {new_main_dc_id}"
        );
        self.main_dc_id.store(new_main_dc_id, Ordering::SeqCst);
    }

    pub fn mark_dc_valid(&self, raw_dc_id: i32) {
        self.dcs.lock().entry(raw_dc_id).or_default().is_valid = true;
    }

    pub fn mark_dc_inited(&self, raw_dc_id: i32) {
        let mut dcs = self.dcs.lock();
        let dc = dcs.entry(raw_dc_id).or_default();
        dc.is_valid = true;
        dc.is_inited = true;
    }

    pub fn is_dc_inited(&self, raw_dc_id: i32) -> bool {
        self.dcs
            .lock()
            .get(&raw_dc_id)
            .is_some_and(|dc| dc.is_inited)
    }

    pub fn stop(&self) {
        self.stop_flag.store(true, Ordering::SeqCst);
    }

    /// `NetQueryDispatcher::dispatch` — the exact C++ routing order.
    pub fn dispatch(&self, mut net_query: NetQuery) {
        if self.check_stop_flag(&mut net_query) {
            return;
        }

        if net_query.is_ready() && net_query.is_error() {
            let code = net_query.error().code();
            let message = net_query.error().message().to_string();
            if code == 303 {
                self.try_fix_migrate(&mut net_query);
            } else if code == RESEND_ERROR_CODE {
                net_query.resend(net_query.dc_id());
            } else if code == 420 && message == "FROZEN_METHOD_INVALID" {
                net_query.set_error(Status::error(406, &message));
            } else if code < 0
                || code == 500
                || (code == 420
                    && !message.starts_with("STORY_SEND_FLOOD_")
                    && !message.starts_with("PREMIUM_SUB_ACTIVE_UNTIL_"))
            {
                // Sent to the NetQueryDelayer in C++. An unsupported wait
                // parameter (`FLOOD_WAIT_10_20`) becomes a 400 error like
                // in the delayer, and the query falls through to routing.
                if code == 420 && unsupported_wait_parameter(&message) {
                    net_query.set_error(Status::error(400, &message));
                } else {
                    if self.check_stop_flag(&mut net_query) {
                        return;
                    }
                    self.delay(net_query, tdutils::time::now());
                    return;
                }
            }
        }

        if !net_query.is_ready() && net_query.dispatch_ttl == 0 {
            net_query.set_error(Status::error_message("DispatchTtlError"));
        }

        let mut dest_dc_id = net_query.dc_id();
        if dest_dc_id.is_main() {
            dest_dc_id = self.get_main_dc_id();
        }
        if !net_query.is_ready() && self.wait_dc_init(dest_dc_id, true).is_err() {
            net_query.set_error(Status::error_message(&format!(
                "No such dc DcId{{{}}}",
                dest_dc_id.get_value()
            )));
        }

        if net_query.is_ready() {
            return self.complete_net_query(net_query);
        }

        if net_query.dispatch_ttl > 0 {
            net_query.dispatch_ttl -= 1;
        }

        if self.check_stop_flag(&mut net_query) {
            return;
        }
        self.session_sink
            .lock()
            .send(dest_dc_id, net_query.query_type(), net_query);
    }

    /// `dispatch_with_callback`.
    pub fn dispatch_with_callback(
        &self,
        mut net_query: NetQuery,
        callback: Box<dyn NetQueryCallback>,
    ) {
        net_query.set_callback(callback);
        self.dispatch(net_query);
    }

    /// `complete_net_query` — to the callback, else to the `Td` stand-in.
    fn complete_net_query(&self, mut net_query: NetQuery) {
        match net_query.move_callback() {
            Some(mut callback) => callback.on_result(net_query),
            None => self.td_sink.lock().on_result(net_query),
        }
    }

    /// `check_stop_flag` — the query completes with the C++
    /// `request_aborted_error` (500, "Request aborted").
    fn check_stop_flag(&self, net_query: &mut NetQuery) -> bool {
        if self.stop_flag.load(Ordering::Relaxed) {
            net_query.set_error(request_aborted_error());
            let net_query = std::mem::replace(net_query, dangling_query());
            self.complete_net_query(net_query);
            return true;
        }
        false
    }

    /// `try_fix_migrate` — FILE_MIGRATE_x and the main-DC migrations.
    fn try_fix_migrate(&self, net_query: &mut NetQuery) {
        let error_message = net_query.error().message().to_string();
        const FILE_MIGRATE_PREFIX: &str = "FILE_MIGRATE_";
        if let Some(new_dc_id) = parse_suffix(&error_message, FILE_MIGRATE_PREFIX) {
            if !DcId::is_valid_raw(new_dc_id) {
                return;
            }
            net_query.resend(DcId::internal(new_dc_id));
            return;
        }
        for prefix in ["PHONE_MIGRATE_", "NETWORK_MIGRATE_", "USER_MIGRATE_"] {
            if let Some(new_main_dc_id) = parse_suffix(&error_message, prefix) {
                self.set_main_dc_id(new_main_dc_id);
                if !net_query.dc_id().is_main() {
                    net_query.resend(DcId::internal(new_main_dc_id));
                } else {
                    net_query.resend(net_query.dc_id());
                }
                break;
            }
        }
    }

    /// `wait_dc_init` — readiness only; the async init request belongs to
    /// Track G (`ConfigManager`/`DcAuthManager`).
    fn wait_dc_init(&self, dc_id: DcId, _force: bool) -> std::result::Result<(), Status> {
        if !dc_id.is_exact() {
            return Err(Status::error_message("Not exact DC"));
        }
        let dcs = self.dcs.lock();
        let Some(dc) = dcs.get(&dc_id.get_raw_id()) else {
            return Err(Status::error_message("Invalid DC"));
        };
        if !dc.is_valid {
            return Err(Status::error_message("Invalid DC"));
        }
        if !dc.is_inited {
            return Err(Status::error_message("DC not inited"));
        }
        Ok(())
    }

    /// The `NetQueryDelayer::delay` policy: parse the wait seconds from
    /// the error message and park the query until it expires.
    fn delay(&self, net_query: NetQuery, now: f64) {
        let code = net_query.error().code();
        let message = net_query.error().message().to_string();
        let delay = if code == 500 && message == "WORKER_BUSY_TOO_LONG_RETRY" {
            // It is dangerous to resend without a timeout.
            1.0
        } else if code == 420 {
            parse_wait_seconds(&message).unwrap_or(1.0)
        } else {
            1.0
        };
        self.delayer.lock().push(DelayedQuery {
            ready_at: now + delay,
            delay,
            query: net_query,
        });
    }

    /// Re-dispatch every delayed query whose deadline (against `now`)
    /// has passed. The C++ delayer actor does this from its timeouts; the
    /// Track F caller drives it (Track G owns the actor wiring).
    pub fn retry_ready_delayed(&self, now: f64) -> usize {
        let mut ready = Vec::new();
        let mut delayer = self.delayer.lock();
        // Split off the due entries, leaving the rest parked.
        let mut due_index = delayer.partition_point(|delayed| delayed.ready_at <= now);
        while due_index > 0 {
            due_index -= 1;
            let delayed = delayer.swap_remove(due_index);
            ready.push(delayed.query);
        }
        drop(delayer);
        let count = ready.len();
        for mut query in ready {
            query.resend(query.dc_id());
            self.dispatch(query);
        }
        count
    }

    pub fn delayed_count(&self) -> usize {
        self.delayer.lock().len()
    }
}

/// The `NetQueryDelayer` wait-parameter prefixes and their clamped
/// values (`clamp(x, 1, 14 * 24 * 60 * 60)`).
fn parse_wait_seconds(message: &str) -> Option<f64> {
    const PREFIXES: [&str; 5] = [
        "FLOOD_WAIT_",
        "SLOWMODE_WAIT_",
        "2FA_CONFIRM_WAIT_",
        "TAKEOUT_INIT_DELAY_",
        "FLOOD_PREMIUM_WAIT_",
    ];
    PREFIXES
        .iter()
        .find_map(|prefix| parse_suffix(message, prefix))
        .map(|seconds| (seconds as f64).clamp(1.0, 14.0 * 24.0 * 60.0 * 60.0))
}

/// The delayer's "unsupported error" check: a second underscore in the
/// wait parameter means the suffix is not a plain number.
fn unsupported_wait_parameter(message: &str) -> bool {
    const PREFIXES: [&str; 5] = [
        "FLOOD_WAIT_",
        "SLOWMODE_WAIT_",
        "2FA_CONFIRM_WAIT_",
        "TAKEOUT_INIT_DELAY_",
        "FLOOD_PREMIUM_WAIT_",
    ];
    PREFIXES.iter().any(|prefix| {
        message
            .strip_prefix(prefix)
            .is_some_and(|suffix| suffix.contains('_'))
    })
}

/// A placeholder for queries moved out of a `&mut` slot on their way to
/// completion (the C++ code moves `NetQueryPtr` freely).
fn dangling_query() -> NetQuery {
    NetQuery::new(
        0,
        DcId::empty(),
        NetQueryType::Common,
        AuthFlag::Off,
        GzipFlag::Off,
        0,
        Vec::new(),
        0,
    )
}

/// `Global::request_aborted_error`.
pub fn request_aborted_error() -> Status {
    Status::error(500, "Request aborted")
}

/// `begins_with(prefix)` + `to_integer<int32>` of the remainder.
fn parse_suffix(message: &str, prefix: &str) -> Option<i32> {
    let suffix = message.strip_prefix(prefix)?;
    suffix
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .and_then(|digits| digits.parse::<i32>().ok())
}
