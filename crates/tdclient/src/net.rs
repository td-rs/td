//! The S76 walking-skeleton session bridge: the live MTProto path behind
//! the `Td` actor. The C++ runs the whole net stack as actors
//! (`td/telegram/net/Session.{h,cpp}` + `SessionMultiProxy` + `DcAuthManager`
//! + `MultiSequenceDispatcher`); those land with the Track I manager wave,
//! behind the S52 `SessionSink`/`TdSink` plug-in points. This module is the
//! documented interim: one dedicated OS thread with its own tokio runtime
//! driving a single [`SessionConnection`] per DC (the exact machinery the
//! S66 `mtproto_auth` gate proved live), a blocking query API for the actor
//! side (the actor thread parks on a condvar — the C++ keeps the actor
//! running and completes via promises; with the single-client walking
//! skeleton the observable JSON behavior is identical), and updates
//! forwarded into the actor mailbox over a captured
//! [`SchedulerPtr`](tdactor::SchedulerPtr).
//!
//! Every API query rides `invokeWithLayer{initConnection{...}}` — without it
//! the DC answers RPC_ERROR CONNECTION_NOT_INITED (the S66 finding). Layer
//! 229 = td 1.8.67 (`td/telegram/Version.h` MTPROTO_LAYER).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::{Condvar, Mutex};
use tdactor::{ActorId, SchedulerPtr, send_closure_later};
use tdmtproto::auth_data::{AuthData, ServerSalt};
use tdmtproto::auth_key::AuthKey;
use tdmtproto::dh_callback::DhCallback;
use tdmtproto::dh_handshake::DhHandshake;
use tdmtproto::handshake::{AuthKeyHandshake, AuthKeyHandshakeContext};
use tdmtproto::handshake_actor::run_handshake;
use tdmtproto::message_id::MessageId;
use tdmtproto::persistence::{load_auth_data, save_auth_data};
use tdmtproto::proxy_secret::ProxySecret;
use tdmtproto::raw_connection::RawConnection;
use tdmtproto::rsa::{PublicRsaKeyInterface, Rsa, RsaKey};
use tdmtproto::session_connection::{SessionConnection, SessionConnectionCallback, SessionMode};
use tdmtproto::telegram_api;
use tdmtproto::transport_type::{TransportType, TransportTypeEnum};
use tdutils::bignum::{BigNum, BigNumContext};
use tdutils::buffer::BufferSlice;
use tdutils::random;
use tdutils::status::{Result, Status};
use tokio::net::TcpStream;
use tokio::sync::mpsc::UnboundedSender;

use crate::auth_manager::{
    AuthNetQuery, AuthNetResult, AuthQuerySender, AuthenticationCodeInfo, CodeType, SentCodeKind,
    WaitPasswordState,
};
use crate::td::Td;

/// The tdlib test-DC key (`PublicRsaKeySharedMain.cpp`).
const TEST_DC_KEY: &str = "-----BEGIN RSA PUBLIC KEY-----\n\
MIIBCgKCAQEAyMEdY1aR+sCR3ZSJrtztKTKqigvO/vBfqACJLZtS7QMgCGXJ6XIR\n\
yy7mx66W0/sOFa7/1mAZtEoIokDP3ShoqF4fVNb6XeqgQfaUHd8wJpDWHcR2OFwv\n\
plUUI1PLTktZ9uW2WE23b+ixNwJjJGwBDJPQEQFBE+vfmH0JP503wr5INS1poWg/\n\
j25sIWeYPHYeOrFp/eXaqhISP6G+q2IeTaWTXpwZj4LzXq5YOpk4bYEQ6mvRq7D1\n\
aHWfYmlEGepfaYR8Q0YqvvhYtMte3ITnuJJs171+GDqpdKcSwHnd6FudwGO4pcCO\n\
j4WcDuXc2CTHgH8gFTNhp/Y8/SpDOhvn4QIDAQAB\n\
-----END RSA PUBLIC KEY-----";

/// The Telegram test DCs (the `p_q_inner_data_dc` field and the obfuscation
/// dc are 10000 + n, main keys) — `test/mtproto.cpp`'s table.
const TEST_DCS: [&str; 3] = ["149.154.175.10", "149.154.167.40", "149.154.175.117"];

/// (ip, handshake dc, transport dc) for a test DC number 1..=3.
fn test_dc(dc: u8) -> (&'static str, i32, i16) {
    let ip = TEST_DCS[(dc as usize - 1).clamp(0, 2)];
    (ip, 10000 + dc as i32, (10000 + dc as i32) as i16)
}

/// The MTPROTO layer of td 1.8.67 (`td/telegram/Version.h`).
const MTPROTO_LAYER: i32 = 229;

/// The walking-skeleton query budget: an RPC that answers nothing within
/// this window fails the query (the `NetQuery` dispatch-ttl analog).
const QUERY_TIMEOUT: Duration = Duration::from_secs(90);

/// The persisted-bridge file name under the tdlib database directory.
pub const AUTH_STATE_FILE: &str = "tdrs_auth.bin";
const STATE_MAGIC: &[u8; 4] = b"TRS1";

// ---------------------------------------------------------------------------
// The actor-side handle
// ---------------------------------------------------------------------------

/// The answer of a blocking [`NetBridge::query`]: the (already
/// gzip-decompressed) result packet bytes, or the RPC error.
pub enum QueryAnswer {
    Ok(Vec<u8>),
    Err(i32, String),
}

struct Slots {
    next_id: u64,
    map: std::collections::HashMap<u64, Option<QueryAnswer>>,
}

struct Shared {
    slots: Mutex<Slots>,
    ready: Condvar,
}

enum Command {
    Query {
        id: u64,
        function: telegram_api::Function,
    },
    Migrate {
        dc: u8,
    },
    /// Marks the current session's key as authorized — the `Td` actor sends
    /// it when the authorization turns Ready (the C++ `AuthData` auth flag).
    MarkAuthorized,
    Persist,
    Shutdown,
}

/// The clone-able query/maintenance surface of the bridge — the half the
/// `Td` actor hands to per-request senders while keeping `&mut self` for
/// its own state (the C++ ActorId separation).
#[derive(Clone)]
pub struct BridgeHandle {
    tx: UnboundedSender<Command>,
    shared: Arc<Shared>,
}

impl BridgeHandle {
    /// Runs one API query to completion. Blocks the calling actor thread —
    /// see the module docs for the S76 interim reasoning.
    pub fn query(&self, function: telegram_api::Function) -> QueryAnswer {
        let id = {
            let mut slots = self.shared.slots.lock();
            let id = slots.next_id;
            slots.next_id += 1;
            slots.map.insert(id, None);
            id
        };
        if self.tx.send(Command::Query { id, function }).is_err() {
            return QueryAnswer::Err(500, "NETWORK_BRIDGE_CLOSED".to_string());
        }
        let mut slots = self.shared.slots.lock();
        let deadline = std::time::Instant::now() + QUERY_TIMEOUT;
        loop {
            if let Some(answer) = slots.map.remove(&id) {
                return answer
                    .unwrap_or_else(|| QueryAnswer::Err(500, "NETWORK_BRIDGE_CLOSED".to_string()));
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                slots.map.remove(&id);
                return QueryAnswer::Err(500, "TIMEOUT".to_string());
            }
            let result = self.shared.ready.wait_for(&mut slots, deadline - now);
            if result.timed_out() {
                slots.map.remove(&id);
                return QueryAnswer::Err(500, "TIMEOUT".to_string());
            }
        }
    }

    /// Moves the bridge to another DC (the `PHONE_MIGRATE_n` /
    /// `NETWORK_MIGRATE_n` fixup — `NetQueryDispatcher`'s 303 handling).
    pub fn migrate(&self, dc: u8) {
        let _ = self.tx.send(Command::Migrate { dc });
    }

    /// Marks the current session authorized (after `authorizationStateReady`).
    pub fn mark_authorized(&self) {
        let _ = self.tx.send(Command::MarkAuthorized);
    }

    /// Persists the (salt/time-refreshed) authorized auth state.
    pub fn persist(&self) {
        let _ = self.tx.send(Command::Persist);
    }

    fn complete(shared: &Shared, id: u64, answer: QueryAnswer) {
        let mut slots = shared.slots.lock();
        if let Some(slot) = slots.map.get_mut(&id) {
            *slot = Some(answer);
        }
        drop(slots);
        shared.ready.notify_all();
    }
}

/// The bridge thread owner: spawns/stops the thread, hands out handles.
pub struct NetBridge {
    handle: BridgeHandle,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl NetBridge {
    /// Spawns the bridge thread. `td` is the actor updates are forwarded to;
    /// `sched` is captured on the actor's thread (`Scheduler::instance_ptr`)
    /// so the foreign thread can send into the actor system.
    pub fn spawn(config: BridgeConfig, td: ActorId<Td>, sched: SchedulerPtr) -> Self {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            slots: Mutex::new(Slots {
                next_id: 1,
                map: std::collections::HashMap::new(),
            }),
            ready: Condvar::new(),
        });
        let thread_shared = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("tdrs-net-bridge".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("failed to build the bridge runtime");
                runtime.block_on(bridge_main(config, td, sched, rx, thread_shared));
            })
            .expect("failed to spawn the bridge thread");
        Self {
            handle: BridgeHandle { tx, shared },
            thread: Some(thread),
        }
    }

    /// The clone-able query surface.
    pub fn handle(&self) -> BridgeHandle {
        self.handle.clone()
    }

    /// Whether a persisted authorized state exists for the database
    /// directory (the C++ AuthManager ctor reading the binlog "auth" key).
    pub fn has_persisted_state(database_directory: &std::path::Path) -> bool {
        load_persisted(database_directory).is_some()
    }

    pub fn shutdown(mut self) {
        let _ = self.handle.tx.send(Command::Persist);
        let _ = self.handle.tx.send(Command::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

// ---------------------------------------------------------------------------
// The bridge thread
// ---------------------------------------------------------------------------

/// The `setTdlibParameters` half the bridge needs.
#[derive(Clone)]
pub struct BridgeConfig {
    pub api_id: i32,
    pub api_hash: String,
    pub use_test_dc: bool,
    /// The tdlib database directory — the persisted auth state lives at
    /// `<database_directory>/tdrs_auth.bin`.
    pub database_directory: PathBuf,
    pub device_model: String,
    pub system_version: String,
    pub application_version: String,
    pub system_language_code: String,
}

/// `invokeWithLayer{initConnection{query}}` with the setTdlibParameters
/// device fields (the C++ `SessionConnection` init wrap).
fn wrap_query(config: &BridgeConfig, query: telegram_api::Function) -> Vec<u8> {
    let mut out = Vec::new();
    telegram_api::invokeWithLayer {
        layer: MTPROTO_LAYER,
        query: Some(Box::new(telegram_api::Function::initConnection(
            telegram_api::initConnection {
                flags: 0,
                api_id: config.api_id,
                device_model: config.device_model.clone(),
                system_version: config.system_version.clone(),
                app_version: config.application_version.clone(),
                system_lang_code: config.system_language_code.clone(),
                lang_pack: String::new(),
                lang_code: config.system_language_code.clone(),
                proxy: Some(telegram_api::InputClientProxy {
                    address: String::new(),
                    port: 0,
                }),
                params: Some(Box::new(telegram_api::JSONValue::jsonNull(
                    telegram_api::jsonNull {},
                ))),
                query: Some(Box::new(query)),
            },
        ))),
    }
    .store_boxed(&mut out);
    out
}

struct RsaInterface {
    rsa: Rsa,
}

impl PublicRsaKeyInterface for RsaInterface {
    fn get_rsa_key(&self, fingerprints: &[i64]) -> Result<RsaKey> {
        let fingerprint = self.rsa.get_fingerprint();
        if fingerprints.contains(&fingerprint) {
            Result::Ok(RsaKey {
                rsa: self.rsa.clone(),
                fingerprint,
            })
        } else {
            Result::Err(Status::error_message("No matching RSA key"))
        }
    }

    fn drop_keys(&mut self) {}
}

struct HandshakeContext {
    rsa: RsaInterface,
}

impl AuthKeyHandshakeContext for HandshakeContext {
    fn get_dh_callback(&self) -> &dyn DhCallback {
        static CALLBACK: PrimeCallback = PrimeCallback;
        &CALLBACK
    }

    fn get_public_rsa_key_interface(&mut self) -> &mut dyn PublicRsaKeyInterface {
        &mut self.rsa
    }
}

/// Accepts the built-in Telegram prime, checks others on the fly.
struct PrimeCallback;

impl DhCallback for PrimeCallback {
    fn is_good_prime(&self, _prime_str: &[u8]) -> i32 {
        -1 // run the full primality checks
    }
    fn add_good_prime(&self, _prime_str: &[u8]) {}
    fn add_bad_prime(&self, _prime_str: &[u8]) {}
}

/// The session callback: routes query answers into the slot map and
/// forwards updates into the `Td` actor mailbox.
struct BridgeCallback {
    td: ActorId<Td>,
    sched: SchedulerPtr,
    shared: Arc<Shared>,
    /// qid by message id — filled by `send`, drained on answers.
    pending: std::collections::HashMap<i64, u64>,
    closed: bool,
}

impl BridgeCallback {
    fn forward_update(&self, data: &[u8]) {
        // The guard installs this thread's scheduler context for the send
        // (the send-only, non-locking form — the C++ `get_const_guard`).
        let _guard = self.sched.guard(false);
        send_closure_later(
            self.td.clone(),
            crate::td::TdMessage::OnNetUpdate {
                data: data.to_vec(),
            },
        );
    }

    fn answer_pending(&mut self, message_id: MessageId, answer: QueryAnswer) {
        if let Some(id) = self.pending.remove(&(message_id.get() as i64)) {
            BridgeHandle::complete(&self.shared, id, answer);
        }
    }
}

impl SessionConnectionCallback for BridgeCallback {
    fn on_connected(&mut self) {}
    fn on_closed(&mut self, _status: Status) {
        self.closed = true;
    }
    fn on_server_salt_updated(&mut self) {}
    fn on_server_time_difference_updated(&mut self, _force: bool) {}
    fn on_new_session_created(&mut self, _unique_id: u64, _first_message_id: MessageId) {}
    fn on_session_failed(&mut self, _status: Status) {
        // The parked queries fail below; the next one re-opens the session
        // over the persisted key (the C++ Session reconnection).
        self.closed = true;
    }
    fn on_container_sent(
        &mut self,
        _container_message_id: MessageId,
        _message_ids: Vec<MessageId>,
    ) {
    }
    fn on_pong(&mut self, _ping_time: f64, _pong_time: f64, _current_time: f64) -> Status {
        Status::ok()
    }
    fn on_update(&mut self, packet: BufferSlice) -> Status {
        self.forward_update(packet.as_slice());
        Status::ok()
    }
    fn on_message_ack(&mut self, _message_id: MessageId) {}
    fn on_message_result_ok(
        &mut self,
        message_id: MessageId,
        packet: BufferSlice,
        _original_size: usize,
    ) -> Status {
        self.answer_pending(message_id, QueryAnswer::Ok(packet.as_slice().to_vec()));
        Status::ok()
    }
    fn on_message_result_error(&mut self, message_id: MessageId, code: i32, message: String) {
        self.answer_pending(message_id, QueryAnswer::Err(code, message));
    }
    fn on_message_failed(&mut self, message_id: MessageId, _status: Status) {
        // The C++ resends failed queries; the bridge answers locally — the
        // caller decides whether to retry (the auth sender migrates).
        self.answer_pending(
            message_id,
            QueryAnswer::Err(500, "MESSAGE_FAILED".to_string()),
        );
    }
    fn on_message_info(
        &mut self,
        _message_id: MessageId,
        _state: i32,
        _answer_message_id: MessageId,
        _answer_size: i32,
        _source: i32,
    ) {
    }
    fn on_destroy_auth_key(&mut self) -> Status {
        Status::ok()
    }
}

/// The open session of the current DC.
struct Live {
    dc: u8,
    connection: SessionConnection,
    /// The key is bound to a user (persisted after this turns true).
    authorized: bool,
}

/// The persisted state: (dc, wall anchor, S61 blob).
type Persisted = (u8, f64, Vec<u8>);

async fn bridge_main(
    config: BridgeConfig,
    td: ActorId<Td>,
    sched: SchedulerPtr,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<Command>,
    shared: Arc<Shared>,
) {
    let mut callback = BridgeCallback {
        td,
        sched,
        shared,
        pending: std::collections::HashMap::new(),
        closed: false,
    };
    let mut persisted: Option<Persisted> = load_persisted(&config.database_directory);
    let mut live: Option<Live> = None;

    loop {
        // 1. Wait for a command (the only wake source when no session is
        //    open) or, with a session, for I/O — then handle.
        let mut inbox: Vec<Command> = Vec::new();
        if let Some(session) = live.as_mut() {
            // Drain first (answers, acks, updates) and compute the deadline.
            let wakeup_at = session.connection.flush(&mut callback).await;
            if callback.closed || wakeup_at == 0.0 {
                fail_pending(&mut callback);
                live = None;
                continue;
            }
            let deadline = tokio::time::Instant::now()
                + Duration::from_secs_f64((wakeup_at - tdutils::time::now()).max(0.0));
            {
                let session = live.as_mut().expect("live");
                tokio::select! {
                    command = rx.recv() => {
                        if let Some(command) = command {
                            inbox.push(command);
                        } else {
                            inbox.push(Command::Shutdown);
                        }
                    }
                    _ = session.connection.raw_connection().stream().readable() => {}
                    _ = tokio::time::sleep_until(deadline) => {}
                }
            }
        } else {
            match rx.recv().await {
                Some(command) => inbox.push(command),
                None => inbox.push(Command::Shutdown),
            }
        }

        // 2. Handle the commands outside the session borrow (they may
        //    reconfigure the session).
        for command in inbox {
            match command {
                Command::Query { id, function } => {
                    run_query(
                        &config,
                        &mut callback,
                        &mut persisted,
                        &mut live,
                        id,
                        function,
                    )
                    .await;
                }
                Command::Migrate { dc } => {
                    close_live(&config, &mut callback, &mut persisted, &mut live);
                    // Keep the persisted key only when it belongs to the
                    // target DC.
                    persisted = load_persisted(&config.database_directory);
                    if persisted.as_ref().is_some_and(|(pdc, _, _)| *pdc != dc) {
                        persisted = None;
                    }
                }
                Command::MarkAuthorized => {
                    if let Some(session) = live.as_mut() {
                        session.authorized = true;
                    }
                }
                Command::Persist => persist(&config, &mut persisted, &mut live),
                Command::Shutdown => {
                    close_live(&config, &mut callback, &mut persisted, &mut live);
                    return;
                }
            }
        }
    }
}

/// Runs one query: opens the session when needed, wraps it in
/// `invokeWithLayer{initConnection{...}}`, sends it, and records the
/// message-id → qid mapping (the answer completes the slot from the
/// callback).
async fn run_query(
    config: &BridgeConfig,
    callback: &mut BridgeCallback,
    persisted: &mut Option<Persisted>,
    live: &mut Option<Live>,
    id: u64,
    function: telegram_api::Function,
) {
    if live.is_none() {
        match open_session(config, persisted.clone()).await {
            Ok(session) => *live = Some(session),
            Err(error) => {
                BridgeHandle::complete(&callback.shared, id, QueryAnswer::Err(500, error));
                return;
            }
        }
    }
    let session = live.as_mut().expect("session just opened");
    let packet = wrap_query(config, function);
    let message_id = session.connection.send_query(
        BufferSlice::from_slice_bytes(&packet),
        false,
        MessageId::default(),
        Vec::new(),
        false,
    );
    callback.pending.insert(message_id.get() as i64, id);
    let _ = session.connection.flush(callback).await;
    if callback.closed {
        // The write failed outright — fail everything parked on this session.
        fail_pending(callback);
        *live = None;
    }
}

fn close_live(
    config: &BridgeConfig,
    callback: &mut BridgeCallback,
    persisted: &mut Option<Persisted>,
    live: &mut Option<Live>,
) {
    persist(config, persisted, live);
    fail_pending(callback);
    *live = None;
}

/// Fails every query parked on the current session.
fn fail_pending(callback: &mut BridgeCallback) {
    let pending = std::mem::take(&mut callback.pending);
    for (_, id) in pending {
        BridgeHandle::complete(
            &callback.shared,
            id,
            QueryAnswer::Err(500, "SESSION_CLOSED".to_string()),
        );
    }
    callback.closed = false;
}

/// Connects + handshakes on a test DC (the `mtproto_auth::establish` leg).
async fn establish(ip: &str, hs_dc: i32, tx_dc: i16) -> Result<(RawConnection, AuthKey, f64, u64)> {
    let stream = match TcpStream::connect((ip, 443u16)).await {
        Ok(stream) => stream,
        Err(error) => {
            return Result::Err(Status::error(500, &format!("connect: {error}")));
        }
    };
    let transport_type = TransportType::new(TransportTypeEnum::Tcp, tx_dc, ProxySecret::default());
    let rsa = match Rsa::from_pem_public_key(TEST_DC_KEY) {
        Result::Ok(rsa) => rsa,
        Result::Err(status) => return Result::Err(status),
    };
    let mut context = HandshakeContext {
        rsa: RsaInterface { rsa },
    };
    let mut handshake = AuthKeyHandshake::new(hs_dc, 0);
    let raw_connection =
        match run_handshake(stream, transport_type, &mut context, 30.0, &mut handshake).await {
            Result::Ok(connection) => connection,
            Result::Err(status) => return Result::Err(status),
        };
    let auth_key = handshake.release_auth_key();
    let time_diff = handshake.get_server_time_diff();
    let salt = handshake.get_server_salt();
    Result::Ok((raw_connection, auth_key, time_diff, salt))
}

/// Opens the session for the current DC: the persisted key when present (an
/// encrypted session WITHOUT a new handshake — the S61 persistence proof),
/// a fresh handshake otherwise.
async fn open_session(
    _config: &BridgeConfig,
    persisted: Option<Persisted>,
) -> std::result::Result<Live, String> {
    if let Some((dc, wall_anchor, blob)) = persisted {
        let (key, _handshake, salt, time_diff, _session_id) = match load_auth_data(&blob) {
            Result::Ok(loaded) => loaded,
            Result::Err(status) => {
                return Err(format!("auth state: {}", status.message()));
            }
        };
        let mut auth_data = AuthData::new();
        auth_data.set_use_pfs(false);
        auth_data.set_main_auth_key(key);
        auth_data.set_server_salt(salt.salt, tdutils::time::now());
        // Re-base the persisted diff onto THIS process's monotonic clock
        // (S66: `time::now()` is process-anchored).
        let diff = time_diff + wall_minus_monotonic() - wall_anchor;
        auth_data.reset_server_time_difference(diff);
        auth_data.set_session_id(fresh_session_id());
        auth_data.set_auth_flag(true);
        let (ip, _hs_dc, tx_dc) = test_dc(dc);
        let stream = TcpStream::connect((ip, 443u16))
            .await
            .map_err(|error| format!("connect: {error}"))?;
        let raw_connection = RawConnection::new(
            stream,
            TransportType::new(TransportTypeEnum::Tcp, tx_dc, ProxySecret::default()),
            None,
        );
        let mut connection = SessionConnection::new(SessionMode::Tcp, raw_connection, auth_data);
        connection.set_online(true, true);
        return Ok(Live {
            dc,
            connection,
            authorized: true,
        });
    }

    // Fresh key: connect to DC 2 first (the C++ starts on the default DC
    // and migrates on PHONE_MIGRATE — the config main DC for the test set).
    let dc = 2;
    let (ip, hs_dc, tx_dc) = test_dc(dc);
    let (raw_connection, auth_key, time_diff, salt) = match establish(ip, hs_dc, tx_dc).await {
        Result::Ok(established) => established,
        Result::Err(status) => return Err(status.message().to_string()),
    };
    let mut auth_data = AuthData::new();
    auth_data.set_main_auth_key(auth_key);
    auth_data.set_use_pfs(false);
    auth_data.reset_server_time_difference(time_diff);
    auth_data.set_server_salt(salt as i64, tdutils::time::now());
    auth_data.set_session_id(fresh_session_id());
    auth_data.set_auth_flag(true);
    let connection = SessionConnection::new(SessionMode::Tcp, raw_connection, auth_data);
    Ok(Live {
        dc,
        connection,
        authorized: false,
    })
}

/// A fresh session id per incarnation, exactly like the C++
/// `Session::start_up` (the id is never persisted; the auth KEY is) —
/// reusing a persisted id trips "SeqNo is too high" on the first content
/// message (the S66 finding).
fn fresh_session_id() -> u64 {
    let mut session_id = 0u64;
    while session_id == 0 {
        session_id = random::secure_uint64();
    }
    session_id
}

/// Wall-clock epoch seconds minus `time::now()` — the re-basing anchor.
fn wall_minus_monotonic() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
        - tdutils::time::now()
}

/// Persists the current session's authorized auth state (the `MTA2`-style
/// layout of `mtproto_auth`, in the tdlib database directory).
fn persist(config: &BridgeConfig, _persisted: &mut Option<Persisted>, live: &mut Option<Live>) {
    let Some(session) = live.as_mut() else { return };
    if !session.authorized {
        return;
    }
    let now = tdutils::time::now();
    let auth_data = session.connection.auth_data();
    let salt = ServerSalt {
        salt: auth_data.get_server_salt(now),
        valid_since: now,
        valid_until: now + 1.0e9,
    };
    let blob = save_auth_data(
        auth_data.get_main_auth_key(),
        None,
        &salt,
        auth_data.get_server_time_difference(),
        auth_data.get_session_id() as i64,
    );
    let dc = session.dc;
    let anchor = wall_minus_monotonic();
    let _ = std::fs::create_dir_all(&config.database_directory);
    let path = config.database_directory.join(AUTH_STATE_FILE);
    let mut out = Vec::new();
    out.extend_from_slice(STATE_MAGIC);
    out.push(dc);
    out.extend_from_slice(&anchor.to_le_bytes());
    out.extend_from_slice(&(blob.len() as u32).to_le_bytes());
    out.extend_from_slice(&blob);
    let _ = std::fs::write(&path, out);
}

fn load_persisted(database_directory: &std::path::Path) -> Option<Persisted> {
    let data = std::fs::read(database_directory.join(AUTH_STATE_FILE)).ok()?;
    if data.len() < 17 || &data[..4] != STATE_MAGIC {
        return None;
    }
    let dc = data[4];
    let wall_anchor = f64::from_le_bytes(data[5..13].try_into().ok()?);
    let blob_len = u32::from_le_bytes(data[13..17].try_into().ok()?) as usize;
    if data.len() < 17 + blob_len {
        return None;
    }
    Some((dc, wall_anchor, data[17..17 + blob_len].to_vec()))
}

// ---------------------------------------------------------------------------
// The auth sender: AuthQuerySender over the bridge
// ---------------------------------------------------------------------------

/// The [`AuthQuerySender`] backed by the live bridge — the C++
/// `AuthManager`'s net-query dispatch (sendCode/signIn/checkPassword, the
/// account.getPassword step-up on SESSION_PASSWORD_NEEDED, and the
/// PHONE/NETWORK_MIGRATE retry).
pub struct AuthSender<'a> {
    bridge: &'a BridgeHandle,
    api_id: i32,
    api_hash: String,
    /// The cloud password (`checkAuthenticationPassword` carries it; the
    /// SRP math runs here, as in the C++ PasswordManager).
    password: String,
    /// The self user of the last `auth.authorization` (the `Td` actor
    /// broadcasts `updateUser(self)` from it before Ready).
    last_user: Option<telegram_api::user>,
}

impl<'a> AuthSender<'a> {
    pub fn new(bridge: &'a BridgeHandle, api_id: i32, api_hash: String, password: String) -> Self {
        Self {
            bridge,
            api_id,
            api_hash,
            password,
            last_user: None,
        }
    }

    /// The self user of the last successful authorization.
    pub fn last_user(&self) -> Option<&telegram_api::user> {
        self.last_user.as_ref()
    }

    /// One query through the bridge, with the PHONE/NETWORK_MIGRATE retry
    /// (the `NetQueryDispatcher` 303 fixup for the auth flow).
    fn invoke(
        &mut self,
        function: telegram_api::Function,
    ) -> std::result::Result<Vec<u8>, (i32, String)> {
        let function = function;
        for _ in 0..2 {
            match self.bridge.query(function.clone()) {
                QueryAnswer::Ok(data) => return Ok(data),
                QueryAnswer::Err(code, message) => {
                    if let Some(dc) = migrate_target(&message) {
                        self.bridge.migrate(dc);
                        // The reconnect happens lazily on the next query;
                        // rebuild the retry copy (the query was consumed).
                        std::thread::sleep(Duration::from_millis(100));
                        continue;
                    }
                    return Err((code, message));
                }
            }
        }
        Err((500, "migrate loop exhausted".to_string()))
    }

    /// `account.getPassword` → the SRP answer for the stored password (the
    /// C++ `PasswordManager::get_input_check_password` step).
    fn srp_check(&mut self) -> std::result::Result<telegram_api::inputCheckPasswordSRP, String> {
        let query = telegram_api::Function::account_getPassword(telegram_api::account_getPassword);
        let data = self.invoke(query).map_err(|(_, message)| message)?;
        let msg = telegram_api::account_Password::parse(&data)
            .map_err(|error| format!("account.password: {error}"))?;
        match &msg.current_algo {
            Some(telegram_api::PasswordKdfAlgo::passwordKdfAlgoSHA256SHA256PBKDF2HMACSHA512iter100000SHA256ModPow(algo)) => {
                calc_input_check_password_srp(self.password.as_bytes(), algo, &msg.srp_B, msg.srp_id)
                    .ok_or_else(|| "invalid SRP parameters".to_string())
            }
            _ => Err("unsupported password algorithm".to_string()),
        }
    }
}

fn migrate_target(message: &str) -> Option<u8> {
    message
        .strip_prefix("PHONE_MIGRATE_")
        .or_else(|| message.strip_prefix("NETWORK_MIGRATE_"))
        .and_then(|n| n.parse().ok())
}

impl AuthQuerySender for AuthSender<'_> {
    fn send(&mut self, query: AuthNetQuery) -> std::result::Result<AuthNetResult, String> {
        match query {
            AuthNetQuery::SendCode { phone_number } => {
                let send_code =
                    telegram_api::Function::auth_sendCode(telegram_api::auth_sendCode {
                        phone_number,
                        api_id: self.api_id,
                        api_hash: self.api_hash.clone(),
                        settings: Some(telegram_api::CodeSettings {
                            flags: 0,
                            allow_flashcall: false,
                            current_number: false,
                            allow_app_hash: false,
                            allow_missed_call: false,
                            allow_firebase: false,
                            unknown_number: false,
                            logout_tokens: Vec::new(),
                            token: String::new(),
                            app_sandbox: false,
                        }),
                    });
                let data = self.invoke(send_code).map_err(|(_, message)| message)?;
                match telegram_api::auth_SentCode::parse(&data)
                    .map_err(|error| format!("auth.sentCode: {error}"))?
                {
                    telegram_api::auth_SentCode::auth_sentCode(msg) => {
                        let (kind, code_info, next_code_info) = sent_code_info(&msg);
                        Ok(AuthNetResult::SentCode {
                            phone_code_hash: msg.phone_code_hash,
                            kind,
                            code_info,
                            next_code_info,
                            timeout: msg.timeout as f64,
                        })
                    }
                    telegram_api::auth_SentCode::auth_sentCodeSuccess(_) => {
                        // `auth.sentCodeSuccess` embeds an authorization —
                        // the number is already logged in (the C++ routes it
                        // straight to on_get_authorization).
                        Err("already authorized via sentCodeSuccess".to_string())
                    }
                    telegram_api::auth_SentCode::auth_sentCodePaymentRequired(_) => {
                        Ok(AuthNetResult::SentCode {
                            phone_code_hash: String::new(),
                            kind: SentCodeKind::PaymentRequired,
                            code_info: Default::default(),
                            next_code_info: Default::default(),
                            timeout: 0.0,
                        })
                    }
                }
            }
            AuthNetQuery::SignIn {
                phone_number,
                phone_code_hash,
                code,
            } => {
                // flags bit 0: `phone_code` is a `flags.0?string` — the
                // generated store gates it on the flag (the S66 trap).
                let sign_in = telegram_api::Function::auth_signIn(telegram_api::auth_signIn {
                    flags: 1,
                    phone_number,
                    phone_code_hash,
                    phone_code: code,
                    email_verification: Some(
                        telegram_api::EmailVerification::emailVerificationCode(
                            telegram_api::emailVerificationCode::default(),
                        ),
                    ),
                });
                match self.invoke(sign_in) {
                    Ok(data) => {
                        let msg = telegram_api::auth_authorization::parse(&data)
                            .map_err(|error| format!("auth.authorization: {error}"))?;
                        let (user_id, user) = match &msg.user {
                            Some(telegram_api::User::user(user)) => {
                                (Some(user.id), Some(user.clone()))
                            }
                            _ => (None, None),
                        };
                        self.last_user = user;
                        Ok(AuthNetResult::Authorization {
                            user_id,
                            terms_of_service: None,
                        })
                    }
                    Err((_, message)) => match message.as_str() {
                        "PHONE_NUMBER_UNOCCUPIED" => Ok(AuthNetResult::Authorization {
                            user_id: None,
                            terms_of_service: None,
                        }),
                        "SESSION_PASSWORD_NEEDED" => {
                            let state = self.password_state()?;
                            Ok(AuthNetResult::PasswordNeeded {
                                wait_password_state: state,
                            })
                        }
                        other => Err(other.to_string()),
                    },
                }
            }
            AuthNetQuery::CheckPassword { password, .. } => {
                self.password = password;
                let check = self.srp_check()?;
                let query =
                    telegram_api::Function::auth_checkPassword(telegram_api::auth_checkPassword {
                        password: Some(telegram_api::InputCheckPasswordSRP::inputCheckPasswordSRP(
                            check,
                        )),
                    });
                match self.invoke(query) {
                    Ok(data) => {
                        let msg = telegram_api::auth_authorization::parse(&data)
                            .map_err(|error| format!("auth.authorization: {error}"))?;
                        let _ = &msg.user;
                        Ok(AuthNetResult::PasswordOk)
                    }
                    Err((_, message)) => Err(message),
                }
            }
            AuthNetQuery::LogOut => {
                let query = telegram_api::Function::auth_logOut(telegram_api::auth_logOut);
                match self.invoke(query) {
                    Ok(_) => Ok(AuthNetResult::LoggedOut),
                    Err((_, message)) => Err(message),
                }
            }
            AuthNetQuery::GetPassword => {
                let state = self.password_state()?;
                Ok(AuthNetResult::PasswordNeeded {
                    wait_password_state: state,
                })
            }
        }
    }
}

impl AuthSender<'_> {
    /// `account.getPassword` → the `WaitPasswordState` fields (the
    /// `authorizationStateWaitPassword` payload).
    fn password_state(&mut self) -> std::result::Result<WaitPasswordState, String> {
        let query = telegram_api::Function::account_getPassword(telegram_api::account_getPassword);
        let data = self.invoke(query).map_err(|(_, message)| message)?;
        let msg = telegram_api::account_Password::parse(&data)
            .map_err(|error| format!("account.password: {error}"))?;
        let (g, p, salt1, salt2) = match &msg.current_algo {
            Some(telegram_api::PasswordKdfAlgo::passwordKdfAlgoSHA256SHA256PBKDF2HMACSHA512iter100000SHA256ModPow(algo)) => {
                (algo.g, algo.p.clone(), algo.salt1.clone(), algo.salt2.clone())
            }
            _ => (0, Vec::new(), Vec::new(), Vec::new()),
        };
        let email_pattern = if msg.email_unconfirmed_pattern.is_empty() {
            msg.login_email_pattern.clone()
        } else {
            msg.email_unconfirmed_pattern.clone()
        };
        Ok(WaitPasswordState {
            current_client_salt: hex(&salt1),
            current_server_salt: hex(&salt2),
            srp_g: g,
            srp_p: hex(&p),
            srp_b: hex(&msg.srp_B),
            srp_id: msg.srp_id,
            hint: msg.hint.clone(),
            has_recovery: msg.has_recovery,
            email_address_pattern: email_pattern,
            has_secure_values: msg.has_secure_values,
        })
    }
}

fn hex(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect()
}

/// `PasswordManager::get_input_check_password` (PasswordManager.cpp:73) —
/// the SRP-6a check for `auth.checkPassword` (the S66 verbatim port).
#[allow(clippy::too_many_lines)]
fn calc_input_check_password_srp(
    password: &[u8],
    algo: &telegram_api::passwordKdfAlgoSHA256SHA256PBKDF2HMACSHA512iter100000SHA256ModPow,
    srp_b: &[u8],
    srp_id: i64,
) -> Option<telegram_api::inputCheckPasswordSRP> {
    let (client_salt, server_salt, p_bytes) = (&algo.salt1, &algo.salt2, &algo.p);
    if DhHandshake::check_config(algo.g, p_bytes, &PrimeCallback).is_error() {
        return None;
    }
    let p = BigNum::from_binary(p_bytes);
    let b = BigNum::from_binary(srp_b);
    let zero = BigNum::new();
    if BigNum::compare(&zero, &b) != -1
        || BigNum::compare(&b, &p) != -1
        || !(248..=256).contains(&srp_b.len())
    {
        return None;
    }

    let mut ctx = BigNumContext::new();
    let mut g = BigNum::new();
    g.set_value(algo.g as u32);
    let g_padded = g.to_binary(256);

    let x = calc_password_hash(password, client_salt, server_salt);
    let x_bn = BigNum::from_binary(&x);

    let mut a_bytes = vec![0u8; 256];
    random::secure_bytes(&mut a_bytes);
    let a_bn = BigNum::from_binary(&a_bytes);

    let a_out = BigNum::mod_exp(&g, &a_bn, &p, &mut ctx).to_binary(256);

    let b_pad = vec![0u8; 256 - srp_b.len()];
    let u = BigNum::from_binary(&tdutils::crypto::sha256(&[&a_out, &b_pad, srp_b].concat()));
    let k = BigNum::from_binary(&tdutils::crypto::sha256(
        &[p_bytes.as_slice(), g_padded.as_slice()].concat(),
    ));

    let v = BigNum::mod_exp(&g, &x_bn, &p, &mut ctx);
    let kv = BigNum::mod_mul(&k, &v, &p, &mut ctx);
    let mut t = BigNum::sub(&b, &kv);
    if BigNum::compare(&t, &zero) == -1 {
        t = BigNum::add(&t, &p);
    }
    let ux = BigNum::mul(&u, &x_bn, &mut ctx);
    let exp = BigNum::add(&ux, &a_bn);

    let s = BigNum::mod_exp(&t, &exp, &p, &mut ctx).to_binary(256);
    let k_hash = tdutils::crypto::sha256(&s);
    let h1 = tdutils::crypto::sha256(p_bytes);
    let h2 = tdutils::crypto::sha256(&g_padded);
    let mut h1x = [0u8; 32];
    for i in 0..32 {
        h1x[i] = h1[i] ^ h2[i];
    }

    let m1 = tdutils::crypto::sha256(
        &[
            h1x.as_slice(),
            tdutils::crypto::sha256(client_salt).as_slice(),
            tdutils::crypto::sha256(server_salt).as_slice(),
            a_out.as_slice(),
            b_pad.as_slice(),
            srp_b,
            k_hash.as_slice(),
        ]
        .concat(),
    );
    Some(telegram_api::inputCheckPasswordSRP {
        srp_id,
        A: a_out,
        M1: m1.to_vec(),
    })
}

/// `PasswordManager::calc_password_hash` (td/telegram/PasswordManager.cpp).
fn calc_password_hash(password: &[u8], client_salt: &[u8], server_salt: &[u8]) -> [u8; 32] {
    fn hash_sha256(salt: &[u8], data: &[u8]) -> [u8; 32] {
        tdutils::crypto::sha256(&[salt, data, salt].concat())
    }
    let buf = hash_sha256(client_salt, password);
    let buf = hash_sha256(server_salt, &buf);
    let mut pbkdf = [0u8; 64];
    tdutils::crypto::pbkdf2_sha512(&buf, client_salt, 100000, &mut pbkdf);
    hash_sha256(server_salt, &pbkdf)
}

/// `SendCodeHelper::on_send_code` — the `auth.sentCode` mapping into the
/// helper's two `AuthenticationCodeInfo` slots.
fn sent_code_info(
    msg: &telegram_api::auth_sentCode,
) -> (SentCodeKind, AuthenticationCodeInfo, AuthenticationCodeInfo) {
    use telegram_api::auth_CodeType as Next;
    use telegram_api::auth_SentCodeType as T;

    let convert = |ty: CodeType, length: i32, pattern: &str| AuthenticationCodeInfo {
        ty,
        length,
        push_timeout: 0,
        cloud_project_number: 0,
        pattern: pattern.to_string(),
    };
    let default_ty =
        telegram_api::auth_SentCodeType::auth_sentCodeTypeSms(telegram_api::auth_sentCodeTypeSms {
            length: 5,
        });
    let ty = msg.r#type.as_ref().unwrap_or(&default_ty);
    let info = match ty {
        T::auth_sentCodeTypeApp(t) => convert(CodeType::Message, t.length, ""),
        T::auth_sentCodeTypeSms(t) => convert(CodeType::Sms, t.length, ""),
        T::auth_sentCodeTypeCall(t) => convert(CodeType::Call, t.length, ""),
        T::auth_sentCodeTypeFlashCall(t) => convert(CodeType::FlashCall, 0, &t.pattern),
        T::auth_sentCodeTypeMissedCall(t) => convert(CodeType::MissedCall, t.length, &t.prefix),
        T::auth_sentCodeTypeFirebaseSms(t) => {
            convert(CodeType::FirebaseAndroidSafetyNet, t.length, "")
        }
        T::auth_sentCodeTypeEmailCode(_) => convert(CodeType::None, 0, ""),
        T::auth_sentCodeTypeSetUpEmailRequired(_) => convert(CodeType::None, 0, ""),
        _ => convert(CodeType::None, 0, ""),
    };
    let default_next =
        telegram_api::auth_CodeType::auth_codeTypeSms(telegram_api::auth_codeTypeSms);
    let next = match msg.next_type.as_ref().unwrap_or(&default_next) {
        Next::auth_codeTypeSms(_) => convert(CodeType::Sms, 0, ""),
        Next::auth_codeTypeCall(_) => convert(CodeType::Call, 0, ""),
        Next::auth_codeTypeFlashCall(_) => convert(CodeType::FlashCall, 0, ""),
        Next::auth_codeTypeMissedCall(_) => convert(CodeType::MissedCall, 0, ""),
        Next::auth_codeTypeFragmentSms(_) => convert(CodeType::Fragment, 0, ""),
    };
    let kind = match ty {
        T::auth_sentCodeTypeSetUpEmailRequired(_) => SentCodeKind::EmailAddressRequired,
        T::auth_sentCodeTypeEmailCode(_) => SentCodeKind::EmailCode,
        _ => SentCodeKind::Code,
    };
    (kind, info, next)
}
