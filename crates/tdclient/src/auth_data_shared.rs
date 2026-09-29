//! Port of `td/telegram/net/AuthDataShared.{h,cpp}` + `AuthKeyState.h`
//! (the net step): the per-DC shared authorization state — the binlog
//! pmc keys `auth<raw_dc_id>`/`salt<raw_dc_id>` and the auth-key
//! listeners `DcAuthManager::add_dc` subscribes with
//! (AuthDataShared.cpp:44-75).
//!
//! Divergence (single-owner pmc): the C++ pmc reads/writes happen on
//! whichever scheduler thread touches the shared auth data; the Rust
//! binlog pmc is only touched on the `Td` actor thread, so the pmc
//! image lives in a mutex-guarded snapshot created client-side at
//! `init_dc` (the pmc read the C++ constructor does, AuthDataShared.cpp:38-46)
//! and pmc writes are marshalled to the `Td` actor as
//! [`NetEvent::PersistAuthKey`]/[`NetEvent::PersistSalts`] through the
//! [`NetEventQueue`]. The pmc bytes and the listener semantics are the
//! C++ ones.

use parking_lot::Mutex;
use std::sync::Arc;

use tdactor::{ActorId, SchedulerPtr};
use tddb::binlog_key_value::BinlogKeyValue;
use tdmtproto::auth_data::{ServerSalt, store_server_salt};
use tdmtproto::auth_key::AuthKey;
use tdmtproto::mtproto_api::tl;
use tdnet::net_query::{DcId, NetQuery};

use crate::td::Td;

/// `AuthKeyState` (AuthKeyState.h:16) and `get_auth_key_state`
/// (:18-26): the key emptiness plus the persisted auth flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthKeyState {
    Empty,
    NoAuth,
    Ok,
}

impl std::fmt::Display for AuthKeyState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            AuthKeyState::Empty => "Empty",
            AuthKeyState::NoAuth => "NoAuth",
            AuthKeyState::Ok => "OK",
        };
        write!(f, "{name}")
    }
}

/// `get_auth_key_state` (AuthKeyState.h:18-26).
pub(crate) fn get_auth_key_state(auth_key: &AuthKey) -> AuthKeyState {
    if auth_key.empty() {
        AuthKeyState::Empty
    } else if auth_key.auth_flag() {
        AuthKeyState::Ok
    } else {
        AuthKeyState::NoAuth
    }
}

/// `AuthDataShared::Listener` (AuthDataShared.h:32-39): `notify()`
/// returns `false` when the listener is dead and must be dropped.
pub(crate) trait AuthDataListener: Send {
    fn notify(&mut self) -> bool;
}

/// The events the session drivers and the shared auth data hand to the
/// `Td` actor (the C++ `G()`/pmc/actor hops). Drained by
/// `Td::drain_net_events` at the actor's pump points.
pub(crate) enum NetEvent {
    /// `Session::on_message_result_ok`'s raw `on_update` forward — the
    /// serialized `telegram_api::Updates` packet.
    UpdatePacket(Vec<u8>),
    /// `SessionConnection::on_server_time_difference_updated` →
    /// `AuthDataShared::update_server_time_difference` → the `Global`
    /// write, for the raw DC.
    ServerTimeDifference {
        raw_dc_id: i32,
        diff: f64,
        force: bool,
    },
    /// `AuthDataShared::set_auth_key`'s pmc write, for the raw DC (the
    /// `DcAuthManager` listeners fire separately).
    PersistAuthKey {
        raw_dc_id: i32,
    },
    /// `AuthDataShared::set_future_salts`'s pmc write, for the raw DC.
    PersistSalts {
        raw_dc_id: i32,
    },
    /// `NetQueryDispatcher::set_main_dc_id`'s binlog half
    /// (NetQueryDispatcher.cpp:444).
    PersistMainDc(i32),
    /// `NetQueryDispatcher::set_main_dc_id`'s `DcAuthManager::
    /// update_main_dc` fan-out (:442-443).
    MainDcChanged(i32),
    /// `NetQueryDispatcher::wait_dc_init`'s `DcAuthManager::add_dc`
    /// (NetQueryDispatcher.cpp:264-266), for the raw DC.
    DcAdded {
        raw_dc_id: i32,
    },
    /// An `AuthDataShared` listener ping — `DcAuthManager::
    /// update_auth_key_state`, for the raw DC.
    AuthKeyChanged {
        raw_dc_id: i32,
    },
    /// The mtproto `ConnectionManager` token pair: `inc_connect` when a
    /// raw connection is handed to its session (ConnectionCreator.cpp:956,
    /// `set_connection_token`) and `dec_connect` when the connection
    /// drops. The Td's `StateManager` port counts them — `Ready` iff the
    /// count is positive.
    ConnectOpened,
    ConnectClosed,
    /// `Session::on_message_result_ok`'s sign-in tail: the response was
    /// `auth.authorization`/`auth.loginTokenSuccess`/`auth.sentCodeSuccess`
    /// (Session.cpp:899-910) — `set_main_dc_id(raw_dc_id_)` on the
    /// dispatcher.
    SignInMainDc {
        raw_dc_id: i32,
    },
    /// `Session::on_message_result_error`'s 401 main-DC tail
    /// (Session.cpp:963-967): `auth_data_.set_auth_flag(false)` +
    /// `G()->log_out(message)`.
    Unauthorized {
        message: String,
    },
    /// A callback-less completed query — the C++ `Td::on_result` route
    /// (NetQueryDispatcher.cpp:38-46) through the parked-completions
    /// convention of the M.3.8 stand-in.
    QueryResult(NetQuery),
}

impl std::fmt::Debug for NetEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `NetQuery` carries no `Debug` (an envelope of buffers); the
        // query id identifies it.
        match self {
            NetEvent::UpdatePacket(packet) => f
                .debug_struct("UpdatePacket")
                .field("len", &packet.len())
                .finish(),
            NetEvent::ServerTimeDifference {
                raw_dc_id,
                diff,
                force,
            } => f
                .debug_struct("ServerTimeDifference")
                .field("raw_dc_id", raw_dc_id)
                .field("diff", diff)
                .field("force", force)
                .finish(),
            NetEvent::PersistAuthKey { raw_dc_id } => f
                .debug_struct("PersistAuthKey")
                .field("raw_dc_id", raw_dc_id)
                .finish(),
            NetEvent::PersistSalts { raw_dc_id } => f
                .debug_struct("PersistSalts")
                .field("raw_dc_id", raw_dc_id)
                .finish(),
            NetEvent::PersistMainDc(raw_dc_id) => {
                f.debug_tuple("PersistMainDc").field(raw_dc_id).finish()
            }
            NetEvent::MainDcChanged(raw_dc_id) => {
                f.debug_tuple("MainDcChanged").field(raw_dc_id).finish()
            }
            NetEvent::DcAdded { raw_dc_id } => f
                .debug_struct("DcAdded")
                .field("raw_dc_id", raw_dc_id)
                .finish(),
            NetEvent::AuthKeyChanged { raw_dc_id } => f
                .debug_struct("AuthKeyChanged")
                .field("raw_dc_id", raw_dc_id)
                .finish(),
            NetEvent::ConnectOpened => f.debug_struct("ConnectOpened").finish(),
            NetEvent::ConnectClosed => f.debug_struct("ConnectClosed").finish(),
            NetEvent::SignInMainDc { raw_dc_id } => f
                .debug_struct("SignInMainDc")
                .field("raw_dc_id", raw_dc_id)
                .finish(),
            NetEvent::Unauthorized { message } => f
                .debug_struct("Unauthorized")
                .field("message", message)
                .finish(),
            NetEvent::QueryResult(query) => f
                .debug_struct("QueryResult")
                .field("query_id", &query.id())
                .finish(),
        }
    }
}

/// The driver→`Td` event mailbox (the `send_closure_later(G()->td(), ...)`
/// hop, as a queue the actor drains at its pump points). Every push fires
/// the attached wake so the actor pumps promptly; without one (the
/// net-inert tests) the queue is inert.
#[derive(Clone, Default)]
pub(crate) struct NetEventQueue {
    inner: Arc<QueueInner>,
}

#[derive(Default)]
struct QueueInner {
    events: Mutex<Vec<NetEvent>>,
    /// The `Td` wake — attached by `Td::init` once the scheduler context
    /// is known.
    waker: Mutex<QueueWaker>,
}

/// The wake target cloned out of the `NetShared` (the scheduler and the
/// actor id, so the queue doesn't hold an `Arc` cycle back to the
/// `NetShared`).
#[derive(Clone, Default)]
pub(crate) struct QueueWaker {
    scheduler: Option<SchedulerPtr>,
    td: Option<ActorId<Td>>,
}

impl QueueWaker {
    /// The `Td::init` construction site (`Td` + scheduler context).
    pub(crate) fn new(scheduler: Option<SchedulerPtr>, td: Option<ActorId<Td>>) -> Self {
        Self { scheduler, td }
    }

    fn wake(&self) {
        let (Some(scheduler), Some(td)) = (self.scheduler.as_ref(), self.td.as_ref()) else {
            return;
        };
        let _guard = scheduler.guard(false);
        tdactor::send_closure_later(td.clone(), crate::td::TdMessage::DrainNetEvents);
    }
}

impl NetEventQueue {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::default(),
        }
    }

    /// Attach the wake target (`Td::init`, where the scheduler context is
    /// known).
    pub(crate) fn set_waker(&self, waker: QueueWaker) {
        *self.inner.waker.lock() = waker;
    }

    pub(crate) fn push(&self, event: NetEvent) {
        self.inner.events.lock().push(event);
        // Clone out (the lock must not be held across the wake).
        let waker = self.inner.waker.lock().clone();
        waker.wake();
    }

    pub(crate) fn take(&self) -> Vec<NetEvent> {
        std::mem::take(&mut self.inner.events.lock())
    }
}

/// The pmc-image snapshot shared with the session drivers: the auth key,
/// the future salts and the raw `Global` server-time difference.
#[derive(Debug, Clone, Default)]
pub(crate) struct AuthDataSnapshot {
    pub auth_key: AuthKey,
    pub future_salts: Vec<ServerSalt>,
    pub server_time_difference: f64,
}

/// `td::AuthDataShared` — one per (internal) DC, shared by the DC's
/// session drivers and the `DcAuthManager`.
pub(crate) struct AuthDataShared {
    dc_id: DcId,
    state: Mutex<AuthDataSnapshot>,
    listeners: Mutex<Vec<Box<dyn AuthDataListener>>>,
    events: NetEventQueue,
}

impl AuthDataShared {
    /// `AuthDataShared::create` with the initial pmc image the caller
    /// (`Td`'s `init_dc`) loaded from `auth<raw>`/`salt<raw>`.
    pub(crate) fn new(dc_id: DcId, initial: AuthDataSnapshot, events: NetEventQueue) -> Arc<Self> {
        let shared = Arc::new(Self {
            dc_id,
            state: Mutex::new(initial),
            listeners: Mutex::new(Vec::new()),
            events,
        });
        let key = shared.get_auth_key();
        tracing::info!(
            "{:?} auth_key_id {} state {}",
            dc_id,
            key.id(),
            get_auth_key_state(&key)
        );
        shared
    }

    pub(crate) fn dc_id(&self) -> DcId {
        self.dc_id
    }

    /// `get_auth_key` (AuthDataShared.cpp:48-50) — the snapshot read the
    /// pmc read serves in the C++.
    pub(crate) fn get_auth_key(&self) -> AuthKey {
        self.state.lock().auth_key.clone()
    }

    /// `set_auth_key` (:52-57): the pmc set becomes the persist event,
    /// then `notify()`.
    pub(crate) fn set_auth_key(&self, auth_key: AuthKey) {
        {
            let mut state = self.state.lock();
            if state.auth_key.id() == auth_key.id()
                && state.auth_key.auth_flag() == auth_key.auth_flag()
            {
                return;
            }
            state.auth_key = auth_key;
        }
        self.events.push(NetEvent::PersistAuthKey {
            raw_dc_id: self.dc_id.get_raw_id(),
        });
        self.notify();
    }

    /// `get_server_time_difference` (:64-66) — the raw `Global` diff.
    pub(crate) fn get_server_time_difference(&self) -> f64 {
        self.state.lock().server_time_difference
    }

    /// `update_server_time_difference` (:60-62) — the `Global` write is
    /// marshalled; the snapshot mirrors it for the next session open.
    pub(crate) fn update_server_time_difference(&self, diff: f64, force: bool) {
        self.events.push(NetEvent::ServerTimeDifference {
            raw_dc_id: self.dc_id.get_raw_id(),
            diff,
            force,
        });
    }

    /// The `Td`-side mirror write for [`NetEvent::ServerTimeDifference`].
    pub(crate) fn set_server_time_difference(&self, diff: f64) {
        self.state.lock().server_time_difference = diff;
    }

    pub(crate) fn get_future_salts(&self) -> Vec<ServerSalt> {
        self.state.lock().future_salts.clone()
    }

    /// `set_future_salts` (:76-78) — the pmc set becomes the persist
    /// event.
    pub(crate) fn set_future_salts(&self, future_salts: Vec<ServerSalt>) {
        {
            let mut state = self.state.lock();
            if state.future_salts == future_salts {
                return;
            }
            state.future_salts = future_salts;
        }
        self.events.push(NetEvent::PersistSalts {
            raw_dc_id: self.dc_id.get_raw_id(),
        });
    }

    /// `add_auth_key_listener` (:68-74): an initially-alive listener is
    /// kept, a dead one is dropped immediately (the `notify()` return).
    pub(crate) fn add_auth_key_listener(&self, mut listener: Box<dyn AuthDataListener>) {
        if listener.notify() {
            self.listeners.lock().push(listener);
        }
    }

    /// `notify` (:104-110): drop the listeners whose `notify()` returned
    /// false.
    fn notify(&self) {
        self.listeners
            .lock()
            .retain_mut(|listener| listener.notify());
    }
}

/// The binlog keys (AuthDataShared.cpp:96-102).
pub(crate) fn get_auth_key_binlog_key(dc_id: DcId) -> Vec<u8> {
    format!("auth{}", dc_id.get_raw_id()).into_bytes()
}

pub(crate) fn get_future_salts_binlog_key(dc_id: DcId) -> Vec<u8> {
    format!("salt{}", dc_id.get_raw_id()).into_bytes()
}

/// `get_auth_key_for_dc` (:38-46) — the pmc read + `unserialize` (an
/// empty value is the default key; a parse failure is the C++
/// `.ensure()` abort, a `panic!` here).
pub(crate) fn load_auth_key_from_pmc(pmc: &mut BinlogKeyValue, dc_id: DcId) -> AuthKey {
    let stored = pmc.get_raw(&get_auth_key_binlog_key(dc_id));
    if stored.is_empty() {
        return AuthKey::default();
    }
    let mut reader = tl::Reader::new(&stored);
    AuthKey::parse(&mut reader)
        .unwrap_or_else(|error| panic!("Failed to unserialize auth key of {dc_id:?}: {error}"))
}

/// `set_auth_key`'s pmc half (:53).
pub(crate) fn save_auth_key_to_pmc(pmc: &mut BinlogKeyValue, dc_id: DcId, auth_key: &AuthKey) {
    let mut out = Vec::new();
    auth_key.store(&mut out);
    pmc.set_raw(&get_auth_key_binlog_key(dc_id), &out);
}

/// `set_future_salts`/`get_future_salts`'s pmc halves (:76-87) — the
/// `std::vector<ServerSalt>` `serialize` framing (i32 count + elements),
/// with the m417-style hostile-count gate.
pub(crate) fn load_salts_from_pmc(pmc: &mut BinlogKeyValue, dc_id: DcId) -> Vec<ServerSalt> {
    let stored = pmc.get_raw(&get_future_salts_binlog_key(dc_id));
    if stored.is_empty() {
        return Vec::new();
    }
    let mut reader = tl::Reader::new(&stored);
    let count = reader.i32().unwrap_or(0);
    if count < 0 || count as usize > reader.remaining() {
        panic!("Failed to unserialize future salts of {dc_id:?}");
    }
    let mut salts = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let salt = tdmtproto::auth_data::parse_server_salt(&mut reader).unwrap_or_else(|error| {
            panic!("Failed to unserialize future salts of {dc_id:?}: {error}")
        });
        salts.push(salt);
    }
    salts
}

pub(crate) fn save_salts_to_pmc(pmc: &mut BinlogKeyValue, dc_id: DcId, salts: &[ServerSalt]) {
    let mut out = Vec::new();
    tl::__i32(&mut out, salts.len() as i32);
    for salt in salts {
        store_server_salt(&mut out, salt);
    }
    pmc.set_raw(&get_future_salts_binlog_key(dc_id), &out);
}

#[cfg(test)]
mod tests {
    use super::*;
    use tddb::binlog_key_value::BinlogKeyValue;
    use tdmtproto::auth_key::AuthKey;

    fn key(id: u64, auth_flag: bool) -> AuthKey {
        let mut auth_key = AuthKey::new(id, vec![9u8; 256]);
        auth_key.set_auth_flag(auth_flag);
        auth_key
    }

    /// A file-backed pmc: `set_raw` needs the binlog half attached (an
    /// uninitialized `BinlogKeyValue::new()` panics on every write).
    fn temp_pmc(name: &str) -> (BinlogKeyValue, String) {
        let path = std::env::temp_dir()
            .join(format!(
                "td-rs-auth-shared-{name}-{}.binlog",
                std::process::id()
            ))
            .to_string_lossy()
            .into_owned();
        let _ = BinlogKeyValue::destroy(&path);
        let mut pmc = BinlogKeyValue::new();
        pmc.init(&path, tddb::db_key::DbKey::empty()).ensure();
        (pmc, path)
    }

    #[test]
    fn auth_key_pmc_round_trip() {
        let (mut pmc, path) = temp_pmc("auth-key");
        let dc_id = DcId::internal(2);

        // An unset key reads back as the default (empty) key.
        assert!(load_auth_key_from_pmc(&mut pmc, dc_id).empty());

        let auth_key = key(0xaaaa_bbbb_cccc_dddd, true);
        save_auth_key_to_pmc(&mut pmc, dc_id, &auth_key);
        let loaded = load_auth_key_from_pmc(&mut pmc, dc_id);
        assert_eq!(loaded.id(), 0xaaaa_bbbb_cccc_dddd);
        assert!(loaded.auth_flag());

        // A different DC does not see the key.
        assert!(load_auth_key_from_pmc(&mut pmc, DcId::internal(3)).empty());
        BinlogKeyValue::destroy(&path);
    }

    #[test]
    fn future_salts_pmc_round_trip() {
        let (mut pmc, path) = temp_pmc("salts");
        let dc_id = DcId::internal(4);
        assert!(load_salts_from_pmc(&mut pmc, dc_id).is_empty());

        let salts = vec![
            ServerSalt {
                salt: 1,
                valid_since: 100.0,
                valid_until: 200.0,
            },
            ServerSalt {
                salt: -2,
                valid_since: 200.0,
                valid_until: 300.0,
            },
        ];
        save_salts_to_pmc(&mut pmc, dc_id, &salts);
        assert_eq!(load_salts_from_pmc(&mut pmc, dc_id), salts);
        BinlogKeyValue::destroy(&path);
    }

    #[test]
    #[should_panic(expected = "Failed to unserialize future salts")]
    fn hostile_salt_count_is_rejected() {
        let (mut pmc, path) = temp_pmc("hostile-salts");
        let dc_id = DcId::internal(5);
        // A huge count with no payload behind it.
        let mut out = Vec::new();
        tl::__i32(&mut out, i32::MAX);
        pmc.set_raw(&get_future_salts_binlog_key(dc_id), &out);
        load_salts_from_pmc(&mut pmc, dc_id);
        BinlogKeyValue::destroy(&path);
    }

    fn test_shared_auth(raw_dc_id: i32, events: &NetEventQueue) -> Arc<AuthDataShared> {
        AuthDataShared::new(
            DcId::internal(raw_dc_id),
            AuthDataSnapshot::default(),
            events.clone(),
        )
    }

    #[test]
    fn set_auth_key_persists_only_on_change() {
        let events = NetEventQueue::new();
        let shared = test_shared_auth(2, &events);

        shared.set_auth_key(key(0x1, true));
        assert!(matches!(
            events.take().as_slice(),
            [NetEvent::PersistAuthKey { raw_dc_id: 2 }]
        ));

        // The same key (id + flag): no new persist.
        shared.set_auth_key(key(0x1, true));
        assert!(events.take().is_empty());

        // A flag flip is a change (the C++ dedup is `id == old && flag == old`).
        shared.set_auth_key(key(0x1, false));
        assert!(matches!(
            events.take().as_slice(),
            [NetEvent::PersistAuthKey { raw_dc_id: 2 }]
        ));
    }

    #[test]
    fn dead_listeners_are_dropped_on_notify() {
        use std::sync::atomic::{AtomicU32, Ordering};

        struct CountingListener(Arc<AtomicU32>, bool);

        impl AuthDataListener for CountingListener {
            fn notify(&mut self) -> bool {
                self.0.fetch_add(1, Ordering::SeqCst);
                self.1
            }
        }

        let events = NetEventQueue::new();
        let shared = test_shared_auth(1, &events);

        let live_count = Arc::new(AtomicU32::new(0));
        let dead_count = Arc::new(AtomicU32::new(0));
        // `add_auth_key_listener` notifies on registration: the live
        // listener is retained (count 1), the dead one is dropped.
        shared.add_auth_key_listener(Box::new(CountingListener(live_count.clone(), true)));
        shared.add_auth_key_listener(Box::new(CountingListener(dead_count.clone(), false)));
        assert_eq!(live_count.load(Ordering::SeqCst), 1, "the add-time notify");
        assert_eq!(
            dead_count.load(Ordering::SeqCst),
            1,
            "dead listener not retained"
        );

        shared.set_auth_key(key(0x2, true));
        assert_eq!(live_count.load(Ordering::SeqCst), 2);
        assert_eq!(
            dead_count.load(Ordering::SeqCst),
            1,
            "dead listener stayed dropped"
        );

        // The second change reaches only the live listener.
        shared.set_auth_key(key(0x3, true));
        assert_eq!(live_count.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn salts_and_time_difference_mirror_the_shared_state() {
        let events = NetEventQueue::new();
        let shared = test_shared_auth(3, &events);

        let salts = vec![ServerSalt {
            salt: 7,
            valid_since: 1.0,
            valid_until: 2.0,
        }];
        shared.set_future_salts(salts.clone());
        assert_eq!(shared.get_future_salts(), salts);
        assert!(matches!(
            events.take().as_slice(),
            [NetEvent::PersistSalts { raw_dc_id: 3 }]
        ));

        // Unchanged salts persist nothing.
        shared.set_future_salts(salts);
        assert!(events.take().is_empty());

        shared.update_server_time_difference(12.5, true);
        // The event goes out; the snapshot only moves on the Td-side mirror.
        assert!(matches!(
            events.take().as_slice(),
            [NetEvent::ServerTimeDifference { raw_dc_id: 3, diff, force: true }] if *diff == 12.5
        ));
        assert_eq!(shared.get_server_time_difference(), 0.0);
        shared.set_server_time_difference(12.5);
        assert_eq!(shared.get_server_time_difference(), 12.5);
    }
}
