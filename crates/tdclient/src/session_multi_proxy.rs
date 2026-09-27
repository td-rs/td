//! The per-DC session machinery (the net step): `SessionMultiProxy` +
//! `SessionProxy` (`td/telegram/net/SessionMultiProxy.{h,cpp}`,
//! `SessionProxy.{h,cpp}`), the `Session` core loop
//! (`td/telegram/net/Session.cpp` — connection creation, the
//! answer/error routing, the sign-in auth steal, the 401 key-drop
//! ladder) and `PublicRsaKeySharedMain` — running over the S62
//! `SessionConnection` machine on a private tokio runtime, exactly like
//! the M.3.7.4 recovery mini-session and the S76 walking-skeleton
//! bridge it replaces.
//!
//! Documented divergences (v1):
//! * Connections are plain TCP to the first usable (non-ipv6,
//!   non-media, secret-free) dc option for the DC — the C++
//!   `ConnectionCreator` link loop (rtt-decayed statistics, staggered
//!   attempts, media/obfuscated preferences, proxies) is its own slice;
//!   the test-DC `+10000` transport-id offset follows
//!   ConnectionCreator.cpp:628-631.
//! * No PFS: `use_pfs` stays off (the C++ default without the option),
//!   so the `MainAuthKeyRecycler` has no counterpart yet.
//! * A closed connection fails its in-flight queries with the C++
//!   "Failed to get answer"-class 500 instead of transparently
//!   re-sending on the reopened session (the managers' own retry
//!   machines handle the rest); the C++ invalid-UTF-8 error guard is
//!   structurally absent (Rust `String` is valid by construction).
//! * The session-count/pfs option changes (`update_session_count`/
//!   `update_use_pfs`/`update_mtproto_header` fan-outs) re-create
//!   nothing live; the header is captured at connection build.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::time::Duration;

use tdactor::{ActorId, SchedulerPtr};
use tdmtproto::auth_data::AuthData;
use tdmtproto::auth_key::AuthKey;
use tdmtproto::dh_callback::DhCallback;
use tdmtproto::handshake::{AuthKeyHandshake, AuthKeyHandshakeContext};
#[cfg(not(target_family = "wasm"))]
use tdmtproto::handshake_actor::run_handshake;
use tdmtproto::message_id::MessageId;
use tdmtproto::proxy_secret::ProxySecret;
#[cfg(not(target_family = "wasm"))]
use tdmtproto::raw_connection::RawConnection;
use tdmtproto::rsa::{PublicRsaKeyInterface, Rsa, RsaKey};
use tdmtproto::session_connection::{SessionConnection, SessionConnectionCallback, SessionMode};
use tdmtproto::telegram_api;
use tdmtproto::transport_type::{TransportType, TransportTypeEnum};
use tdnet::net_query::{AuthFlag, DcId, DcInitSink, GzipFlag, NetQuery, NetQueryType, SessionSink};
use tdutils::buffer::BufferSlice;
use tdutils::random;
use tdutils::status::{Result, Status};
#[cfg(not(target_family = "wasm"))]
use tokio::net::TcpStream;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::auth_data_shared::AuthDataShared;
use crate::auth_data_shared::{NetEvent, NetEventQueue};
use crate::config_manager::StoredDcOption;
use crate::td::{Td, TdMessage};

// ---------------------------------------------------------------------------
// PublicRsaKeySharedMain
// ---------------------------------------------------------------------------

/// The test-environment public key (`PublicRsaKeySharedMain.cpp:25-32`).
const TEST_PUBLIC_RSA_KEY: &str = "-----BEGIN RSA PUBLIC KEY-----\n\
MIIBCgKCAQEAyMEdY1aR+sCR3ZSJrtztKTKqigvO/vBfqACJLZtS7QMgCGXJ6XIR\n\
yy7mx66W0/sOFa7/1mAZtEoIokDP3ShoqF4fVNb6XeqgQfaUHd8wJpDWHcR2OFwv\n\
plUUI1PLTktZ9uW2WE23b+ixNwJjJGwBDJPQEQFBE+vfmH0JP503wr5INS1poWg/\n\
j25sIWeYPHYeOrFp/eXaqhISP6G+q2IeTaWTXpwZj4LzXq5YOpk4bYEQ6mvRq7D1\n\
aHWfYmlEGepfaYR8Q0YqvvhYtMte3ITnuSJs171+GDqpdKcSwHnd6FudwGO4pcCO\n\
j4WcDuXc2CTHgH8gFTNhp/Y8/SpDOhvn9QIDAQAB\n\
-----END RSA PUBLIC KEY-----";

/// The production public key (`PublicRsaKeySharedMain.cpp:40-47`).
const MAIN_PUBLIC_RSA_KEY: &str = "-----BEGIN RSA PUBLIC KEY-----\n\
MIIBCgKCAQEA6LszBcC1LGzyr992NzE0ieY+BSaOW622Aa9Bd4ZHLl+TuFQ4lo4g\n\
5nKaMBwK/BIb9xUfg0Q29/2mgIR6Zr9krM7HjuIcCzFvDtr+L0GQjae9H0pRB2OO\n\
62cECs5HKhT5DZ98K33vmWiLowc621dQuwKWSQKjWf50XYFw42h21P2KXUGyp2y/\n\
+aEyZ+uVgLLQbRA1dEjSDZ2iGRy12Mk5gpYc397aYp438fsJoHIgJ2lgMv5h7WY9\n\
t6N/byY9Nw9p21Og3AoXSL2q/2IJ1WRUhebgAdGVMlV1fkuOQoEzR7EdpqtQD9Cs\n\
5+bfo3Nhmcyvk5ftB0WkJ9z6bNZ7yxrP8wIDAQAB\n\
-----END RSA PUBLIC KEY-----";

/// `td::PublicRsaKeySharedMain` (`PublicRsaKeySharedMain.cpp:14-67`): the
/// static embedded key set the internal-DC handshakes resolve against.
pub(crate) struct PublicRsaKeySharedMain {
    keys: Vec<(Rsa, i64)>,
}

impl PublicRsaKeySharedMain {
    /// `PublicRsaKeySharedMain::create(is_test)`.
    pub(crate) fn create(is_test: bool) -> Self {
        let pem = if is_test {
            TEST_PUBLIC_RSA_KEY
        } else {
            MAIN_PUBLIC_RSA_KEY
        };
        let rsa = Rsa::from_pem_public_key(pem)
            .unwrap_or_else(|_| panic!("Invalid embedded public RSA key"));
        let fingerprint = rsa.get_fingerprint();
        Self {
            keys: vec![(rsa, fingerprint)],
        }
    }

    fn get_rsa_key_inner(&self, fingerprints: &[i64]) -> Result<RsaKey> {
        for fingerprint in fingerprints {
            for (rsa, key_fingerprint) in &self.keys {
                if key_fingerprint == fingerprint {
                    return Result::Ok(RsaKey {
                        rsa: rsa.clone(),
                        fingerprint: *fingerprint,
                    });
                }
            }
        }
        Result::Err(Status::error(
            400,
            &format!("Unknown Main fingerprints {fingerprints:?}"),
        ))
    }
}

impl PublicRsaKeyInterface for PublicRsaKeySharedMain {
    fn get_rsa_key(&self, fingerprints: &[i64]) -> Result<RsaKey> {
        self.get_rsa_key_inner(fingerprints)
    }

    fn drop_keys(&mut self) {
        // nothing to do
    }
}

/// The `Arc`-front for the handshake context (the interface hands out
/// `&mut dyn`, the keys are shared across drivers).
struct SharedRsaInterface(Arc<PublicRsaKeySharedMain>);

impl PublicRsaKeyInterface for SharedRsaInterface {
    fn get_rsa_key(&self, fingerprints: &[i64]) -> Result<RsaKey> {
        self.0.get_rsa_key_inner(fingerprints)
    }

    fn drop_keys(&mut self) {}
}

/// The official Telegram 2048-bit DH prime: pre-seeded into the
/// good-prime cache. The C++ callback caches after the first (fast,
/// native) safe-prime check; a first check under emscripten costs ~a
/// minute of Miller-Rabin in a debug build, which blows Telegram's
/// handshake window — seeding the one prime everyone uses equals the
/// post-first-check cache state.
const OFFICIAL_DH_PRIME_HEX: &str = "c71caeb9c6b1c9048e6c522f70f13f73980d40238e3e21c14934d037563d930f48198a0aa7c14058229493d22530f4dbfa336f6e0ac925139543aed44cce7c3720fd51f69458705ac68cd4fe6b6b13abdc9746512969328454f18faf8c595f642477fe96bb2a941d5bcd1d4ac8cc49880708fa9b378e3c4f3a9060bee67cf9a4a4a695811051907e162753b56b0f6b410dba74d8a84b2a14b3144e0ef1284754fd17ed950d5965b4b9dd46582db1178d169c6bc465b0d6ff9ca3928fef5b9ae4e418fc15e83ebea0f87fa9ff5eed70050ded2849f47bf959d956850ce929851f0d8115f635b105ee2e4e15d04b2454bf6f4fadf034b10403119cd8e3b92fcc5b";

fn official_prime_bytes() -> Vec<u8> {
    (0..OFFICIAL_DH_PRIME_HEX.len() / 2)
        .map(|i| u8::from_str_radix(&OFFICIAL_DH_PRIME_HEX[2 * i..2 * i + 2], 16).expect("hex"))
        .collect()
}

type PrimeCache = std::sync::Mutex<(
    std::collections::HashSet<Vec<u8>>,
    std::collections::HashSet<Vec<u8>>,
)>;

fn prime_cache() -> &'static PrimeCache {
    static CACHE: std::sync::OnceLock<PrimeCache> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| {
        let cache: PrimeCache = Default::default();
        cache.lock().unwrap().0.insert(official_prime_bytes());
        cache
    })
}

/// The C++ `DhCallback` behind the handshake `Context`: the safe-prime
/// verdicts cache per process (the C++ persists the learned primes).
struct PrimeCallback;

impl DhCallback for PrimeCallback {
    fn is_good_prime(&self, prime_str: &[u8]) -> i32 {
        let cache = prime_cache().lock().unwrap();
        if cache.0.contains(prime_str) {
            return 1;
        }
        if cache.1.contains(prime_str) {
            return 0;
        }
        -1
    }

    fn add_good_prime(&self, prime_str: &[u8]) {
        prime_cache().lock().unwrap().0.insert(prime_str.to_vec());
    }

    fn add_bad_prime(&self, prime_str: &[u8]) {
        prime_cache().lock().unwrap().1.insert(prime_str.to_vec());
    }
}

struct HandshakeContext {
    dh: PrimeCallback,
    rsa: SharedRsaInterface,
}

impl AuthKeyHandshakeContext for HandshakeContext {
    fn get_dh_callback(&self) -> &dyn DhCallback {
        &self.dh
    }

    fn get_public_rsa_key_interface(&mut self) -> &mut dyn PublicRsaKeyInterface {
        &mut self.rsa
    }
}

// ---------------------------------------------------------------------------
// The shared context
// ---------------------------------------------------------------------------

/// Everything the session drivers share with the `Td` actor — the C++
/// `G()` surface, scoped to what the session layer reads.
pub(crate) struct NetShared {
    /// The `Td` actor (event wakeups); `None` outside a scheduler
    /// context (direct-call tests pump the actor manually).
    pub td: Option<ActorId<Td>>,
    /// The cross-thread send handle; `None` outside a scheduler context
    /// (direct-call tests pump the actor manually).
    pub scheduler: Option<SchedulerPtr>,
    pub events: NetEventQueue,
    /// `G()->mtproto_header().get_default_header()` — regenerated on the
    /// option changes (Session.cpp:279).
    pub header: Mutex<Vec<u8>>,
    /// The `ConnectionCreator` dc-option mirror (the merged defaults +
    /// update list, ConnectionCreator.cpp:1012-1037 order).
    pub dc_options: Mutex<Vec<StoredDcOption>>,
    pub is_test_dc: bool,
    /// The dispatcher's main DC (the 401 `can_drop_main_auth_key` read,
    /// Session.cpp:951).
    pub main_dc_id: Arc<AtomicI32>,
    /// The `PublicRsaKeySharedMain::create(G()->is_test_dc())` key set.
    pub public_rsa_key: Arc<PublicRsaKeySharedMain>,
}

impl NetShared {
    /// The `send_closure_later(G()->td(), ...)` hop from a driver thread.
    pub(crate) fn push_event(&self, event: NetEvent) {
        // The queue's push fires the actor wake itself.
        self.events.push(event);
    }

    pub(crate) fn wake_td(&self) {
        let (Some(scheduler), Some(td)) = (self.scheduler.as_ref(), self.td.as_ref()) else {
            return;
        };
        let _guard = scheduler.guard(false);
        tdactor::send_closure_later(td.clone(), TdMessage::DrainNetEvents);
    }

    /// `ConnectionCreator::get_transport_type`'s dc id
    /// (ConnectionCreator.cpp:628-631): the raw id, `+10000` for the
    /// test environment.
    pub(crate) fn transport_dc_id(&self, raw_dc_id: i32) -> i32 {
        if self.is_test_dc {
            raw_dc_id + 10000
        } else {
            raw_dc_id
        }
    }
}

/// Wall-clock epoch seconds minus `time::now()` — the re-basing anchor
/// for the process-anchored monotonic clock (the S66 finding).
pub(crate) fn wall_minus_monotonic() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
        - tdutils::time::now()
}

/// A fresh session id per incarnation — the C++ `Session::start_up`
/// (the id is never persisted; reusing one trips "SeqNo is too high").
fn fresh_session_id() -> u64 {
    let mut session_id = 0;
    while session_id == 0 {
        session_id = random::secure_uint64();
    }
    session_id
}

// ---------------------------------------------------------------------------
// SessionMultiProxy
// ---------------------------------------------------------------------------

/// The `NetQuery::Type` split of the per-DC pools
/// (NetQueryDispatcher.cpp:179-198).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionPoolKind {
    Main,
    Upload,
    Download,
    DownloadSmall,
}

impl SessionPoolKind {
    pub(crate) fn from_query_type(query_type: NetQueryType) -> Self {
        match query_type {
            NetQueryType::Common => SessionPoolKind::Main,
            NetQueryType::Upload => SessionPoolKind::Upload,
            NetQueryType::Download => SessionPoolKind::Download,
            NetQueryType::DownloadSmall => SessionPoolKind::DownloadSmall,
        }
    }
}

/// One `SessionProxy` slot: the query counter plus the driver inbox,
/// materialized with the first query (SessionProxy.cpp:74-95).
struct SessionSlot {
    query_count: Arc<AtomicU64>,
    tx: Option<UnboundedSender<DriverCommand>>,
}

enum DriverCommand {
    Send(NetQuery),
    UpdateMainFlag(bool),
    Close,
}

/// `td::SessionMultiProxy` — the per-(DC, kind) fan-out over
/// `session_count` driver slots (SessionMultiProxy.cpp:39-65 for the
/// pick, :136-167 for the slot creation).
pub(crate) struct SessionPool {
    shared: Arc<NetShared>,
    auth_data: Arc<AuthDataShared>,
    raw_dc_id: i32,
    kind: SessionPoolKind,
    /// `is_main_` — flipped by the dispatcher's main-DC change.
    is_main: Arc<AtomicBool>,
    slots: Mutex<Vec<SessionSlot>>,
}

impl SessionPool {
    fn new(
        shared: Arc<NetShared>,
        auth_data: Arc<AuthDataShared>,
        raw_dc_id: i32,
        kind: SessionPoolKind,
        session_count: usize,
        is_main: bool,
    ) -> Arc<Self> {
        let slots = (0..session_count.max(1))
            .map(|_| SessionSlot {
                query_count: Arc::new(AtomicU64::new(0)),
                tx: None,
            })
            .collect();
        Arc::new(Self {
            shared,
            auth_data,
            raw_dc_id,
            kind,
            is_main: Arc::new(AtomicBool::new(is_main)),
            slots: Mutex::new(slots),
        })
    }

    /// `SessionMultiProxy::send` (SessionMultiProxy.cpp:39-65):
    /// authorized queries spread by the least-loaded slot (the
    /// `session_rand` fast path has no producer yet — the creator
    /// stand-ins leave it 0, so this is the C++ min-count branch),
    /// unauthorized queries always ride slot 0.
    pub(crate) fn send(&self, query: NetQuery) {
        let pos = {
            let slots = self.slots.lock();
            if query.auth_flag() == AuthFlag::On {
                let mut best = 0usize;
                let mut min_count = slots[0].query_count.load(Ordering::Relaxed);
                let mut equal_count = 1u32;
                for (i, slot) in slots.iter().enumerate().skip(1) {
                    let count = slot.query_count.load(Ordering::Relaxed);
                    if count < min_count {
                        best = i;
                        min_count = count;
                        equal_count = 1;
                    } else if count == min_count {
                        equal_count += 1;
                        if random::fast_uint32().is_multiple_of(equal_count) {
                            best = i;
                        }
                    }
                }
                best
            } else {
                0
            }
        };
        let (tx, query_count) = {
            let mut slots = self.slots.lock();
            let slot = &mut slots[pos];
            slot.query_count.fetch_add(1, Ordering::Relaxed);
            let tx = slot.tx.get_or_insert_with(|| {
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<DriverCommand>();
                // One dedicated OS thread per slot — the C++ Session
                // actor's own scheduler slice. The S76-bridge pattern:
                // the SessionConnection/handshake machinery holds raw
                // pointers (`*mut RawConnection`, non-`Send` callback
                // traits), so the driver future must stay on one thread;
                // a current-thread runtime per thread keeps it `Send`-free.
                let shared = self.shared.clone();
                let auth_data = self.auth_data.clone();
                let kind = self.kind;
                let raw_dc_id = self.raw_dc_id;
                let is_main = self.is_main.clone();
                let query_count = slot.query_count.clone();
                let name = format!("td-session-dc{}-{kind:?}", self.raw_dc_id);
                let _ = std::thread::Builder::new().name(name).spawn(move || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("failed to build the session driver runtime");
                    runtime.block_on(session_driver(
                        shared,
                        auth_data,
                        kind,
                        raw_dc_id,
                        is_main,
                        rx,
                        query_count,
                    ));
                });
                tx
            });
            (tx.clone(), slot.query_count.clone())
        };
        let _ = query_count;
        let _ = tx.send(DriverCommand::Send(query));
    }

    /// `SessionMultiProxy::update_main_flag` (:67-73).
    pub(crate) fn update_main_flag(&self, is_main: bool) {
        self.is_main.store(is_main, Ordering::SeqCst);
        let slots = self.slots.lock();
        for slot in slots.iter() {
            if let Some(tx) = slot.tx.as_ref() {
                let _ = tx.send(DriverCommand::UpdateMainFlag(is_main));
            }
        }
    }

    /// `NetQueryDispatcher::stop`'s per-pool reset.
    pub(crate) fn close(&self) {
        let slots = self.slots.lock();
        for slot in slots.iter() {
            if let Some(tx) = slot.tx.as_ref() {
                let _ = tx.send(DriverCommand::Close);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The session driver (SessionProxy + Session)
// ---------------------------------------------------------------------------

/// The open session of one slot — the `Session::main_connection_`.
struct Live {
    connection: SessionConnection,
    /// In-flight queries by their wire message id.
    pending: HashMap<u64, NetQuery>,
}

/// The answer shapes the callback queues for the driver loop (the
/// callback cannot borrow the connection it is flushing — the S76
/// precedent).
enum DriverAnswer {
    Ok {
        message_id: MessageId,
        packet: Vec<u8>,
    },
    Err {
        message_id: MessageId,
        code: i32,
        message: String,
    },
    Failed(MessageId),
}

/// The `SessionCallback` (Session.cpp:886-1000): routes query answers
/// into the pending map and forwards updates into the shared context.
struct DriverCallback {
    shared: Arc<NetShared>,
    raw_dc_id: i32,
    answers: Vec<DriverAnswer>,
    /// The message ids whose answer stole authorization
    /// (Session.cpp:899-910).
    sign_ins: Vec<MessageId>,
    /// `on_server_salt_updated` fired (Session.cpp:612-618).
    salts_updated: bool,
    /// `on_server_time_difference_updated` fired with the force flag.
    time_diff_updated: Option<bool>,
    closed: bool,
}

impl SessionConnectionCallback for DriverCallback {
    fn on_connected(&mut self) {}

    fn on_closed(&mut self, _status: Status) {
        self.closed = true;
    }

    fn on_server_salt_updated(&mut self) {
        self.salts_updated = true;
    }

    fn on_server_time_difference_updated(&mut self, force: bool) {
        self.time_diff_updated = Some(force);
    }

    fn on_new_session_created(&mut self, _unique_id: u64, _first_message_id: MessageId) {}

    fn on_session_failed(&mut self, _status: Status) {}

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
        self.shared
            .events
            .push(NetEvent::UpdatePacket(packet.as_slice().to_vec()));
        Status::ok()
    }

    fn on_message_ack(&mut self, _message_id: MessageId) {}

    fn on_message_result_ok(
        &mut self,
        message_id: MessageId,
        packet: BufferSlice,
        _original_size: usize,
    ) -> Status {
        // Only the sign-in detection needs the packet here; the answer
        // processing (which touches the local AuthData) runs on the
        // driver after the flush.
        if matches!(
            first_ctor(packet.as_slice()),
            Some(id)
                if id == telegram_api::auth_authorization::ID
                    || id == telegram_api::auth_loginTokenSuccess::ID
                    || id == telegram_api::auth_sentCodeSuccess::ID
        ) {
            self.sign_ins.push(message_id);
        }
        self.answers.push(DriverAnswer::Ok {
            message_id,
            packet: packet.as_slice().to_vec(),
        });
        Status::ok()
    }

    fn on_message_result_error(&mut self, message_id: MessageId, mut code: i32, message: String) {
        // Session.cpp:925-935 — the invalid-code guard (the invalid
        // UTF-8 branch is structural: a Rust String is always UTF-8).
        if code <= -10000 || code >= 10000 || code == 0 {
            code = 500;
        }
        self.answers.push(DriverAnswer::Err {
            message_id,
            code,
            message,
        });
    }

    fn on_message_failed(&mut self, message_id: MessageId, _status: Status) {
        self.answers.push(DriverAnswer::Failed(message_id));
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

/// The first TL constructor of a serialized answer, if present.
fn first_ctor(packet: &[u8]) -> Option<i32> {
    if packet.len() >= 4 {
        Some(i32::from_le_bytes(packet[0..4].try_into().unwrap()))
    } else {
        None
    }
}

/// `Session::on_message_result_ok`'s completion: to the query callback,
/// else into the event queue (the `Td::on_result` route).
fn complete_query(shared: &NetShared, mut query: NetQuery) {
    match query.move_callback() {
        Some(mut callback) => callback.on_result(query),
        None => shared.events.push(NetEvent::QueryResult(query)),
    }
}

/// The driver body: one `SessionProxy` slot — connects, hands the
/// `SessionConnection` machine its I/O, and services the pool's
/// commands (the S76 bridge loop, with the Session.cpp semantics).
async fn session_driver(
    shared: Arc<NetShared>,
    auth_data: Arc<AuthDataShared>,
    kind: SessionPoolKind,
    raw_dc_id: i32,
    is_main: Arc<AtomicBool>,
    mut rx: UnboundedReceiver<DriverCommand>,
    query_count: Arc<AtomicU64>,
) {
    let mut live: Option<Live> = None;
    let mut backoff_until = 0.0f64;
    let _ = kind;

    loop {
        // 1. I/O with the open session, or idle-wait for a command.
        let mut inbox: Vec<DriverCommand> = Vec::new();
        if let Some(session) = live.as_mut() {
            let mut callback = DriverCallback {
                shared: shared.clone(),
                raw_dc_id,
                answers: Vec::new(),
                sign_ins: Vec::new(),
                salts_updated: false,
                time_diff_updated: None,
                closed: false,
            };
            let wakeup_at = session.connection.flush(&mut callback).await;
            process_answers(&shared, &auth_data, session, &mut callback, &query_count);
            if callback.salts_updated {
                // Session.cpp:612-618 — mirror the salts into the shared
                // half (which queues the pmc persist).
                let salts = session.connection.auth_data().get_future_salts();
                auth_data.set_future_salts(salts);
            }
            if let Some(force) = callback.time_diff_updated {
                let diff = session.connection.auth_data().get_server_time_difference();
                auth_data.update_server_time_difference(diff, force);
            }
            if callback.closed || wakeup_at == 0.0 {
                fail_pending(&shared, session, &query_count, "Failed to get answer");
                live = None;
                backoff_until = tdutils::time::now() + 1.0;
                continue;
            }
            let deadline = tokio::time::Instant::now()
                + Duration::from_secs_f64((wakeup_at - tdutils::time::now()).max(0.0));
            tokio::select! {
                command = rx.recv() => {
                    match command {
                        Some(command) => inbox.push(command),
                        None => inbox.push(DriverCommand::Close),
                    }
                }
                _ = session.connection.raw_connection().stream().readable() => {}
                _ = tokio::time::sleep_until(deadline) => {}
            }
        } else {
            match rx.recv().await {
                Some(command) => inbox.push(command),
                None => break,
            }
        }

        // 2. Commands outside the session borrow.
        for command in inbox {
            match command {
                DriverCommand::Send(query) => {
                    if live.is_none() {
                        if tdutils::time::now() < backoff_until {
                            // The SessionProxy backoff: fail instead of
                            // queuing during the cool-down.
                            let mut query = query;
                            query.set_error(Status::error(500, "Failed to get answer"));
                            complete_query(&shared, query);
                            continue;
                        }
                        match open_connection(&shared, &auth_data, raw_dc_id, &is_main).await {
                            Ok(connection) => {
                                live = Some(Live {
                                    connection,
                                    pending: HashMap::new(),
                                })
                            }
                            Err(status) => {
                                backoff_until = tdutils::time::now() + 1.0;
                                let mut query = query;
                                query.set_error(status);
                                complete_query(&shared, query);
                                continue;
                            }
                        }
                    }
                    let session = live.as_mut().expect("session just opened");
                    let message_id = session.connection.send_query(
                        BufferSlice::from_slice_bytes(query.query()),
                        query.gzip_flag() == GzipFlag::On,
                        MessageId::default(),
                        Vec::new(),
                        false,
                    );
                    session.pending.insert(message_id.get(), query);
                }
                DriverCommand::UpdateMainFlag(is_main_flag) => {
                    if let Some(session) = live.as_mut() {
                        session.connection.set_online(true, is_main_flag);
                    }
                }
                DriverCommand::Close => {
                    if let Some(mut session) = live.take() {
                        let mut callback = DriverCallback {
                            shared: shared.clone(),
                            raw_dc_id,
                            answers: Vec::new(),
                            sign_ins: Vec::new(),
                            salts_updated: false,
                            time_diff_updated: None,
                            closed: false,
                        };
                        session.connection.force_close(&mut callback);
                        fail_pending(&shared, &mut session, &query_count, "Request aborted");
                    }
                    return;
                }
            }
        }
    }
}

/// `Session::on_message_result_ok`/`on_message_result_error` bodies —
/// run on the driver after the flush so they can touch the session's
/// local `AuthData` (the C++ callback does it inline through its raw
/// pointer).
fn process_answers(
    shared: &Arc<NetShared>,
    auth_data: &Arc<AuthDataShared>,
    session: &mut Live,
    callback: &mut DriverCallback,
    query_count: &Arc<AtomicU64>,
) {
    let raw_dc_id = callback.raw_dc_id;
    let answers = std::mem::take(&mut callback.answers);
    let sign_ins = std::mem::take(&mut callback.sign_ins);
    let mut events: Vec<NetEvent> = Vec::new();
    for answer in answers {
        let message_id = match answer.message_id() {
            Some(message_id) => message_id,
            None => continue,
        };
        let Some(mut query) = session.pending.remove(&message_id.get()) else {
            continue;
        };
        match answer {
            DriverAnswer::Ok { packet, .. } => {
                let local = session.connection.auth_data();
                local.on_api_response();
                if sign_ins.contains(&message_id)
                    && query.tl_constructor() != telegram_api::auth_importAuthorization::ID
                {
                    // Session.cpp:899-910 — the authorization steal.
                    events.push(NetEvent::SignInMainDc { raw_dc_id });
                    local.set_auth_flag(true);
                    auth_data.set_auth_key(local.get_main_auth_key().clone());
                }
                let packet = if query.gzip_flag() == GzipFlag::On {
                    tdutils::gzip::gzdecode(&packet).as_slice().to_vec()
                } else {
                    packet
                };
                query.set_ok(packet);
            }
            DriverAnswer::Err { code, message, .. } => {
                let mut code = code;
                if code == 401 && message != "SESSION_PASSWORD_NEEDED" {
                    // Session.cpp:951-968 — no PFS, no CDN, so the drop
                    // ladder: a non-main DC loses its key; the main DC
                    // logs out.
                    if shared.main_dc_id.load(Ordering::Relaxed) != raw_dc_id {
                        session.connection.auth_data().drop_main_auth_key();
                        auth_data.set_auth_key(AuthKey::default());
                        code = 500;
                    } else {
                        session.connection.auth_data().set_auth_flag(false);
                        auth_data.set_auth_key(
                            session.connection.auth_data().get_main_auth_key().clone(),
                        );
                        events.push(NetEvent::Unauthorized {
                            message: message.clone(),
                        });
                        // on_session_failed: the connection dies; the
                        // remaining in-flight queries fail with it.
                        fail_pending(shared, session, query_count, "Failed to get answer");
                    }
                }
                if code == 400
                    && (message == "CONNECTION_NOT_INITED" || message == "CONNECTION_LAYER_INVALID")
                {
                    session.connection.auth_data().on_connection_not_inited();
                    code = 500;
                }
                query.set_error(Status::error(code, &message));
            }
            DriverAnswer::Failed(_) => {
                query.set_error(Status::error(500, "Failed to get answer"));
            }
        }
        query_count.fetch_sub(1, Ordering::Relaxed);
        complete_query(shared, query);
    }
    for event in events {
        shared.push_event(event);
    }
}

impl DriverAnswer {
    fn message_id(&self) -> Option<MessageId> {
        match self {
            DriverAnswer::Ok { message_id, .. }
            | DriverAnswer::Err { message_id, .. }
            | DriverAnswer::Failed(message_id) => Some(*message_id),
        }
    }
}

/// Fails every in-flight query of a dying session.
fn fail_pending(
    shared: &NetShared,
    session: &mut Live,
    query_count: &Arc<AtomicU64>,
    message: &str,
) {
    let pending = std::mem::take(&mut session.pending);
    for (_, mut query) in pending {
        query.set_error(Status::error(500, message));
        query_count.fetch_sub(1, Ordering::Relaxed);
        complete_query(shared, query);
    }
}

/// The `ConnectionCreator::create` + `Session::open_connection` v1:
/// first usable option, fresh TCP connection, handshake when the key is
/// empty (the C++ `GenAuthKeyActor` leg).
#[cfg(not(target_family = "wasm"))]
async fn open_connection(
    shared: &Arc<NetShared>,
    auth_data: &Arc<AuthDataShared>,
    raw_dc_id: i32,
    is_main: &AtomicBool,
) -> std::result::Result<SessionConnection, Status> {
    let option = pick_dc_option(shared, raw_dc_id)
        .ok_or_else(|| Status::error(500, &format!("No usable dc option for DC {raw_dc_id}")))?;
    let port =
        u16::try_from(option.port()).map_err(|_| Status::error_message("Bad dc option port"))?;
    let stream = TcpStream::connect((option.ip_address(), port))
        .await
        .map_err(|error| Status::error(500, &format!("connect: {error}")))?;

    let transport_dc = shared.transport_dc_id(raw_dc_id);
    let transport_type = TransportType::new(
        TransportTypeEnum::Tcp,
        transport_dc as i16,
        ProxySecret::default(),
    );
    let now = tdutils::time::now();
    let mut auth_local = AuthData::new();
    auth_local.set_use_pfs(false);
    auth_local.set_session_id(fresh_session_id());
    auth_local.set_header(shared.header.lock().clone());

    let auth_key = auth_data.get_auth_key();
    if auth_key.empty() {
        // The GenAuthKeyActor leg: handshake on this connection, then
        // publish the key (Session.cpp's fetch_main_auth_key path).
        let mut context = HandshakeContext {
            dh: PrimeCallback,
            rsa: SharedRsaInterface(shared.public_rsa_key.clone()),
        };
        let mut handshake = AuthKeyHandshake::new(transport_dc, 0);
        let raw_connection =
            match run_handshake(stream, transport_type, &mut context, 30.0, &mut handshake).await {
                Result::Ok(connection) => connection,
                Result::Err(status) => return Err(status),
            };
        let auth_key = handshake.release_auth_key();
        let time_diff = handshake.get_server_time_diff();
        let salt = handshake.get_server_salt();
        auth_local.set_main_auth_key(auth_key.clone());
        auth_local.reset_server_time_difference(time_diff);
        auth_local.set_server_salt(salt as i64, tdutils::time::now());
        auth_data.set_auth_key(auth_key);
        // The measured diff feeds the Global update (the C++
        // AuthDataShared::update_server_time_difference hop).
        shared.events.push(NetEvent::ServerTimeDifference {
            raw_dc_id,
            diff: time_diff,
            force: false,
        });
        let mut connection = SessionConnection::new(SessionMode::Tcp, raw_connection, auth_local);
        connection.set_online(true, is_main.load(Ordering::Relaxed));
        return Ok(connection);
    }

    auth_local.set_main_auth_key(auth_key);
    // The C++ re-bases the persisted diff onto this process's clock.
    let diff = auth_data.get_server_time_difference() + wall_minus_monotonic();
    auth_local.reset_server_time_difference(diff);
    auth_local.set_future_salts(auth_data.get_future_salts(), now);

    let raw_connection = RawConnection::new(stream, transport_type, None);
    let mut connection = SessionConnection::new(SessionMode::Tcp, raw_connection, auth_local);
    connection.set_online(true, is_main.load(Ordering::Relaxed));
    Ok(connection)
}

/// wasm: browsers cannot open raw TCP sockets to the DCs; the pools fail
/// fast here until the WebSocket transport slice lands.
#[cfg(target_family = "wasm")]
async fn open_connection(
    _shared: &Arc<NetShared>,
    _auth_data: &Arc<AuthDataShared>,
    _raw_dc_id: i32,
    _is_main: &AtomicBool,
) -> std::result::Result<SessionConnection, Status> {
    Err(Status::error_message(
        "Network sessions are unsupported on this platform until the browser transport slice",
    ))
}

/// The `DcOptionsSet::fetch_dc_option` v1 — the first usable
/// (non-ipv6, non-media, secret-free) option for the raw DC, in the
/// C++ add order (defaults first, then the sorted update).
fn pick_dc_option(shared: &NetShared, raw_dc_id: i32) -> Option<StoredDcOption> {
    let options = shared.dc_options.lock();
    options
        .iter()
        .find(|option| {
            option.raw_dc_id() == raw_dc_id
                && !option.is_ipv6()
                && !option.is_media_only()
                && option.secret().is_empty()
        })
        .cloned()
}

// ---------------------------------------------------------------------------
// NetRouter — the dispatcher's SessionSink + DcInitSink
// ---------------------------------------------------------------------------

/// The four pools of one inited DC (NetQueryDispatcher.cpp:251-262).
struct DcPools {
    main: Arc<SessionPool>,
    upload: Arc<SessionPool>,
    download: Arc<SessionPool>,
    download_small: Arc<SessionPool>,
}

/// The dispatcher's routing sink — `dcs_[dc_pos].{main,upload,download,
/// download_small}_session_` (NetQueryDispatcher.cpp:179-198) plus the
/// lazy `wait_dc_init` init body (:227-266).
pub(crate) struct NetRouter {
    shared: Arc<NetShared>,
    /// The per-raw-DC shared auth data (pre-built from the pmc at Td
    /// init).
    registry: HashMap<i32, Arc<AuthDataShared>>,
    /// `get_session_count()` (NetQueryDispatcher.cpp:364-366), captured
    /// at init.
    session_count: usize,
    /// The upload/download counts (:247-250), captured at init.
    upload_session_count: usize,
    download_session_count: usize,
    dcs: Mutex<HashMap<i32, Arc<DcPools>>>,
}

impl NetRouter {
    pub(crate) fn new(
        shared: Arc<NetShared>,
        registry: HashMap<i32, Arc<AuthDataShared>>,
        session_count: i32,
        is_premium: bool,
    ) -> Self {
        let session_count = session_count.max(1) as usize;
        // NetQueryDispatcher.cpp:247-250 (the DC2/DC4 upload nuance
        // rides the premium read; both collapse to the same v1 value
        // without the premium option).
        let upload_session_count = if is_premium { 8 } else { 4 };
        let download_session_count = if is_premium { 8 } else { 2 };
        Self {
            shared,
            registry,
            session_count,
            upload_session_count,
            download_session_count,
            dcs: Mutex::new(HashMap::new()),
        }
    }

    fn pool(&self, raw_dc_id: i32, kind: SessionPoolKind) -> Option<Arc<SessionPool>> {
        let dcs = self.dcs.lock();
        let pools = dcs.get(&raw_dc_id)?;
        Some(match kind {
            SessionPoolKind::Main => pools.main.clone(),
            SessionPoolKind::Upload => pools.upload.clone(),
            SessionPoolKind::Download => pools.download.clone(),
            SessionPoolKind::DownloadSmall => pools.download_small.clone(),
        })
    }

    /// `NetQueryDispatcher::stop`'s pool shutdown.
    pub(crate) fn close(&self) {
        let dcs = self.dcs.lock();
        for pools in dcs.values() {
            pools.main.close();
            pools.upload.close();
            pools.download.close();
            pools.download_small.close();
        }
    }
}

/// The orphan-rule wrapper around the shared router — `SessionSink`/
/// `DcInitSink` are foreign traits, so the `Arc<NetRouter>` hand-off
/// goes through this newtype.
pub(crate) struct RouterSink(pub Arc<NetRouter>);

impl SessionSink for RouterSink {
    fn send(&mut self, dc_id: DcId, query_type: NetQueryType, query: NetQuery) {
        self.0.send(dc_id, query_type, query);
    }
}

impl DcInitSink for RouterSink {
    fn init_dc(&mut self, dc_id: DcId) -> bool {
        self.0.init_dc(dc_id)
    }

    fn update_main_dc(&mut self, new_main_dc_id: i32) {
        self.0.update_main_dc(new_main_dc_id)
    }
}

impl NetRouter {
    /// The `wait_dc_init` init body (NetQueryDispatcher.cpp:227-266):
    /// create the DC's four pools and register the DC with the
    /// `DcAuthManager` (as the `DcAdded` event).
    pub(crate) fn init_dc(&self, dc_id: DcId) -> bool {
        let raw = dc_id.get_raw_id();
        let mut dcs = self.dcs.lock();
        if dcs.contains_key(&raw) {
            return true;
        }
        let Some(auth_data) = self.registry.get(&raw).cloned() else {
            tdutils::log_error!("Cannot init unknown DC {raw}");
            return false;
        };
        let is_main = self.shared.main_dc_id.load(Ordering::Relaxed) == raw;
        let pools = DcPools {
            main: SessionPool::new(
                self.shared.clone(),
                auth_data.clone(),
                raw,
                SessionPoolKind::Main,
                self.session_count,
                is_main,
            ),
            upload: SessionPool::new(
                self.shared.clone(),
                auth_data.clone(),
                raw,
                SessionPoolKind::Upload,
                self.upload_session_count,
                false,
            ),
            download: SessionPool::new(
                self.shared.clone(),
                auth_data.clone(),
                raw,
                SessionPoolKind::Download,
                self.download_session_count,
                false,
            ),
            download_small: SessionPool::new(
                self.shared.clone(),
                auth_data,
                raw,
                SessionPoolKind::DownloadSmall,
                self.download_session_count,
                false,
            ),
        };
        dcs.insert(raw, Arc::new(pools));
        drop(dcs);
        self.shared.push_event(NetEvent::DcAdded { raw_dc_id: raw });
        true
    }

    /// The `NetQueryDispatcher::send` routing arm (:179-198).
    pub(crate) fn send(&self, dc_id: DcId, query_type: NetQueryType, query: NetQuery) {
        let mut dc_id = dc_id;
        if dc_id.is_main() {
            dc_id = DcId::internal(self.shared.main_dc_id.load(Ordering::Relaxed));
        }
        if let Some(pool) = self.pool(
            dc_id.get_raw_id(),
            SessionPoolKind::from_query_type(query_type),
        ) {
            pool.send(query);
        } else {
            // The dispatcher's init gate guarantees the pools; a miss is
            // a close race.
            let mut query = query;
            query.set_error(Status::error_message("No such dc"));
            complete_query(&self.shared, query);
        }
    }

    /// `NetQueryDispatcher::set_main_dc_id`'s tail (:434-444): the old
    /// and new main-pool flags, the `DcAuthManager::update_main_dc`
    /// hop and the binlog persistence.
    pub(crate) fn update_main_dc(&self, new_main_dc_id: i32) {
        let old = self
            .shared
            .main_dc_id
            .swap(new_main_dc_id, Ordering::SeqCst);
        let dcs = self.dcs.lock();
        if let Some(pools) = dcs.get(&old) {
            pools.main.update_main_flag(false);
        }
        if let Some(pools) = dcs.get(&new_main_dc_id) {
            pools.main.update_main_flag(true);
        }
        drop(dcs);
        self.shared
            .events
            .push(NetEvent::PersistMainDc(new_main_dc_id));
        self.shared
            .push_event(NetEvent::MainDcChanged(new_main_dc_id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth_data_shared::{AuthDataShared, AuthDataSnapshot, NetEvent, NetEventQueue};

    fn test_shared() -> Arc<NetShared> {
        Arc::new(NetShared {
            td: None,
            scheduler: None,
            events: NetEventQueue::new(),
            header: Mutex::new(Vec::new()),
            dc_options: Mutex::new(Vec::new()),
            is_test_dc: false,
            main_dc_id: Arc::new(std::sync::atomic::AtomicI32::new(1)),
            public_rsa_key: Arc::new(PublicRsaKeySharedMain::create(false)),
        })
    }

    fn registry_with(shared: &Arc<NetShared>, raw_dc_id: i32) -> HashMap<i32, Arc<AuthDataShared>> {
        let auth_data = AuthDataShared::new(
            DcId::internal(raw_dc_id),
            AuthDataSnapshot::default(),
            shared.events.clone(),
        );
        HashMap::from([(raw_dc_id, auth_data)])
    }

    #[test]
    fn init_dc_refuses_an_unregistered_dc() {
        let shared = test_shared();
        let router = NetRouter::new(shared.clone(), HashMap::new(), 1, false);

        assert!(!router.init_dc(DcId::internal(2)), "no auth data, no pools");
        assert!(
            shared.events.take().is_empty(),
            "the refusal queues nothing"
        );
    }

    #[test]
    fn init_dc_registers_once_and_the_main_dc_update_fans_out() {
        let shared = test_shared();
        let router = NetRouter::new(shared.clone(), registry_with(&shared, 2), 1, false);

        assert!(router.init_dc(DcId::internal(2)));
        // The C++ init is guarded by `is_inited` — a second call is a no-op.
        assert!(router.init_dc(DcId::internal(2)), "idempotent");
        let events = shared.events.take();
        assert_eq!(events.len(), 1, "exactly one DcAdded: {events:?}");
        assert!(matches!(
            events.as_slice(),
            [NetEvent::DcAdded { raw_dc_id: 2 }]
        ));

        router.update_main_dc(2);
        assert_eq!(
            shared.main_dc_id.load(std::sync::atomic::Ordering::SeqCst),
            2
        );
        assert!(matches!(
            shared.events.take().as_slice(),
            [NetEvent::PersistMainDc(2), NetEvent::MainDcChanged(2)]
        ));
    }

    #[test]
    fn pool_kinds_map_from_query_types() {
        assert_eq!(
            SessionPoolKind::from_query_type(NetQueryType::Common),
            SessionPoolKind::Main
        );
        assert_eq!(
            SessionPoolKind::from_query_type(NetQueryType::Upload),
            SessionPoolKind::Upload
        );
        assert_eq!(
            SessionPoolKind::from_query_type(NetQueryType::Download),
            SessionPoolKind::Download
        );
        assert_eq!(
            SessionPoolKind::from_query_type(NetQueryType::DownloadSmall),
            SessionPoolKind::DownloadSmall
        );
    }
}
