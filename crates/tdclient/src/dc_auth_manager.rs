//! Port of `td/telegram/net/DcAuthManager.{h,cpp}` (the net step): the
//! per-DC authorization loop — with a working main DC, every registered
//! DC gets its auth key (the handshake belongs to the session driver)
//! and, when it holds a key without an authorization
//! (`AuthKeyState::NoAuth`), an `auth.exportAuthorization` to the main
//! DC followed by `auth.importAuthorization` to the target DC.
//!
//! The C++ actor runs on the main scheduler and dispatches through
//! `G()->net_query_dispatcher()`; the Rust manager is an inline state
//! machine on the `Td` actor (the M.3.7.5 convention): its two queries
//! dispatch through the same client-side dispatcher with a parking
//! callback, and the `Td` drains the completions back into
//! `DcAuthManager::on_result`.

use parking_lot::Mutex;
use std::sync::Arc;

use tdmtproto::telegram_api;
use tdnet::net_query::{
    AuthFlag, DcId, GzipFlag, NetQuery, NetQueryCallback, NetQueryDispatcher, NetQueryType,
};

use crate::auth_data_shared::{
    AuthDataListener, AuthDataShared, AuthKeyState, NetEvent, get_auth_key_state,
};
use crate::session_multi_proxy::NetShared;

/// `DcInfo::State` (DcAuthManager.h:31-39).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DcState {
    Waiting,
    Export,
    Import,
    BeforeOk,
    Ok,
}

/// `DcAuthManager::DcInfo`.
struct DcInfo {
    dc_id: DcId,
    auth_data: Arc<AuthDataShared>,
    auth_key_state: AuthKeyState,
    state: DcState,
    export_id: i64,
    export_bytes: Vec<u8>,
    /// `dc.wait_id` — the in-flight query id (`MAX` = none).
    wait_id: u64,
}

/// The parked export/import completions — the C++
/// `actor_shared(this, dc.dc_id.get_raw_id())` callback token.
#[derive(Clone, Default)]
pub(crate) struct DcCompletedQueue(Arc<Mutex<Vec<(i32, NetQuery)>>>);

impl DcCompletedQueue {
    pub(crate) fn new() -> Self {
        Self(Arc::new(Mutex::new(Vec::new())))
    }

    fn push(&self, raw_dc_id: i32, query: NetQuery) {
        self.0.lock().push((raw_dc_id, query));
    }

    pub(crate) fn take(&self) -> Vec<(i32, NetQuery)> {
        std::mem::take(&mut self.0.lock())
    }
}

/// The export/import completion callback — the C++
/// `actor_shared(this, dc.dc_id.get_raw_id())` receiver. Parks the answer
/// and nudges the `Td` actor (the C++ closure delivery), which drains the
/// queue into `DcAuthManager::on_result`.
struct DcAuthCallback {
    raw_dc_id: i32,
    completed: DcCompletedQueue,
    shared: Arc<NetShared>,
}

impl NetQueryCallback for DcAuthCallback {
    fn on_result(&mut self, query: NetQuery) {
        self.completed.push(self.raw_dc_id, query);
        self.shared.wake_td();
    }
}

/// The `AuthDataShared::Listener` (DcAuthManager.cpp:46-60): pings the
/// `Td` actor, which re-runs `update_auth_key_state`.
struct DcAuthListener {
    shared: Arc<NetShared>,
    raw_dc_id: i32,
}

impl AuthDataListener for DcAuthListener {
    fn notify(&mut self) -> bool {
        self.shared.push_event(NetEvent::AuthKeyChanged {
            raw_dc_id: self.raw_dc_id,
        });
        true
    }
}

/// `td::DcAuthManager`.
pub(crate) struct DcAuthManager {
    shared: Arc<NetShared>,
    completed: DcCompletedQueue,
    main_dc_id: Option<DcId>,
    dcs: Vec<DcInfo>,
    need_check_authorization_is_ok: bool,
    /// `need_destroy_auth_key_` (the destroy-promise hop is not wired —
    /// `dispatcher.destroy_auth_keys` has no Td-side caller yet).
    need_destroy_auth_key: bool,
}

impl DcAuthManager {
    pub(crate) fn new(shared: Arc<NetShared>, completed: DcCompletedQueue) -> Self {
        Self {
            shared,
            completed,
            main_dc_id: None,
            dcs: Vec::new(),
            need_check_authorization_is_ok: false,
            need_destroy_auth_key: false,
        }
    }

    /// `DcAuthManager::add_dc` (DcAuthManager.cpp:44-75): register the
    /// shared auth data, seed the state from the key, default the main
    /// DC, subscribe the key listener and run the loop.
    pub(crate) fn add_dc(&mut self, auth_data: Arc<AuthDataShared>) {
        let dc_id = auth_data.dc_id();
        let auth_key_state = get_auth_key_state(&auth_data.get_auth_key());
        if self.main_dc_id.is_none() {
            self.main_dc_id = Some(dc_id);
        }
        auth_data.add_auth_key_listener(Box::new(DcAuthListener {
            shared: self.shared.clone(),
            raw_dc_id: dc_id.get_raw_id(),
        }));
        self.dcs.push(DcInfo {
            dc_id,
            auth_data,
            auth_key_state,
            state: DcState::Waiting,
            export_id: -1i64,
            export_bytes: Vec::new(),
            wait_id: u64::MAX,
        });
    }

    /// `DcAuthManager::update_main_dc` (:77-81).
    pub(crate) fn update_main_dc(&mut self, new_main_dc_id: DcId) {
        self.main_dc_id = Some(new_main_dc_id);
    }

    /// `DcAuthManager::update_auth_key_state` (:97-105) — the listener
    /// ping, keyed by the raw DC id.
    pub(crate) fn update_auth_key_state(&mut self, raw_dc_id: i32) {
        let Some(dc) = self.find_dc_mut(raw_dc_id) else {
            return;
        };
        dc.auth_key_state = get_auth_key_state(&dc.auth_data.get_auth_key());
    }

    /// `DcAuthManager::on_result` (:107-143) — the export/import
    /// answers, keyed by the raw DC id.
    pub(crate) fn on_result(&mut self, raw_dc_id: i32, net_query: NetQuery) {
        let Some(dc) = self.find_dc_mut(raw_dc_id) else {
            return;
        };
        if dc.wait_id != net_query.id() {
            return;
        }
        dc.wait_id = u64::MAX;
        match dc.state {
            DcState::Import => {
                let exported = net_query
                    .is_ok()
                    .then(|| telegram_api::auth_ExportedAuthorization::parse(net_query.ok()))
                    .map(|parsed| parsed.ok());
                match exported.flatten() {
                    Some(result) => {
                        dc.export_id = result.id;
                        dc.export_bytes = result.bytes;
                    }
                    None => {
                        dc.state = DcState::Export;
                    }
                }
            }
            DcState::BeforeOk => {
                match net_query
                    .is_ok()
                    .then(|| telegram_api::auth_authorization::parse(net_query.ok()))
                {
                    Some(Ok(_)) => {
                        dc.state = DcState::Ok;
                    }
                    _ => {
                        dc.state = DcState::Export;
                    }
                }
            }
            _ => {}
        }
    }

    /// `DcAuthManager::check_authorization_is_ok` (:247-249). The C++
    /// caller is `NetQueryDispatcher::check_authorization_is_ok`
    /// (NetQueryDispatcher.cpp:447-452), driven by the AuthManager
    /// restore path (AuthManager.cpp:355).
    pub(crate) fn check_authorization_is_ok(&mut self) {
        self.need_check_authorization_is_ok = true;
    }

    /// `DcAuthManager::destroy`'s flag half (:197-201); the completion
    /// promise is unwired (the C++ resolves it when every DC key is
    /// Empty and then destroys the Td). The C++ caller is
    /// `NetQueryDispatcher::destroy_auth_keys` (:316-332), driven by the
    /// AuthManager logout paths (AuthManager.cpp:373, 1459).
    pub(crate) fn destroy(&mut self) {
        self.need_destroy_auth_key = true;
    }

    /// Whether every registered DC lost its key (`destroy_loop`,
    /// :203-220).
    pub(crate) fn destroy_is_ready(&self) -> bool {
        self.dcs
            .iter()
            .all(|dc| dc.auth_key_state == AuthKeyState::Empty)
    }

    /// `DcAuthManager::loop` (:222-245): dispatch the export/import
    /// ladder for every DC that needs it. Callers drain
    /// [`Self::take_completed`] into [`Self::on_result`] afterwards.
    pub(crate) fn loop_(
        &mut self,
        dispatcher: &NetQueryDispatcher,
        next_query_id: impl Fn() -> u64,
    ) {
        if self.need_destroy_auth_key {
            // destroy_loop — the sessions drop their keys through the
            // dispatcher's destroy path; nothing to dispatch here.
            if self.destroy_is_ready() {
                self.need_destroy_auth_key = false;
            }
        }
        let Some(main_dc_id) = self.main_dc_id else {
            return;
        };
        let main_ready = self
            .find_dc(main_dc_id.get_raw_id())
            .is_some_and(|dc| dc.auth_key_state == AuthKeyState::Ok);
        if !main_ready {
            if self.need_check_authorization_is_ok {
                // G()->log_out (DcAuthManager.cpp:235).
                self.need_check_authorization_is_ok = false;
                self.shared.push_event(NetEvent::Unauthorized {
                    message: "Authorization check failed in DcAuthManager".to_string(),
                });
            }
            return;
        }
        self.need_check_authorization_is_ok = false;
        for pos in 0..self.dcs.len() {
            self.dc_loop(pos, dispatcher, &next_query_id);
        }
    }

    /// The parked export/import completions for the `Td` drain.
    pub(crate) fn take_completed(&self) -> Vec<(i32, NetQuery)> {
        self.completed.take()
    }

    /// `DcAuthManager::dc_loop` (:145-195) for `dcs[pos]`.
    fn dc_loop(
        &mut self,
        pos: usize,
        dispatcher: &NetQueryDispatcher,
        next_query_id: impl Fn() -> u64,
    ) {
        let dc = &mut self.dcs[pos];
        if dc.auth_key_state == AuthKeyState::Ok {
            return;
        }
        if dc.state == DcState::Ok {
            // "Lost key in ..., restart dc_loop"
            dc.state = DcState::Waiting;
        }
        match dc.state {
            DcState::Waiting | DcState::Export => {
                // Send auth.exportAuthorization to the main DC.
                let id = next_query_id();
                let mut body = Vec::new();
                telegram_api::auth_exportAuthorization {
                    dc_id: dc.dc_id.get_raw_id(),
                }
                .store_boxed(&mut body);
                let query = NetQuery::new(
                    id,
                    DcId::main(),
                    NetQueryType::Common,
                    AuthFlag::On,
                    GzipFlag::Off,
                    telegram_api::auth_exportAuthorization::ID,
                    body,
                    u32::MAX,
                    60 * 60 * 24,
                );
                let raw_dc_id = dc.dc_id.get_raw_id();
                dispatcher.dispatch_with_callback(
                    query,
                    Box::new(DcAuthCallback {
                        raw_dc_id,
                        completed: self.completed.clone(),
                        shared: self.shared.clone(),
                    }),
                );
                dc.wait_id = id;
                dc.export_id = -1;
                dc.state = DcState::Import;
            }
            DcState::Import => {
                if dc.export_id == -1 {
                    return;
                }
                // Send auth.importAuthorization to the DC (the C++
                // AuthFlag::Off — DcAuthManager.cpp:183).
                let id = next_query_id();
                let mut body = Vec::new();
                telegram_api::auth_importAuthorization {
                    id: dc.export_id,
                    bytes: std::mem::take(&mut dc.export_bytes),
                }
                .store_boxed(&mut body);
                let raw_dc_id = dc.dc_id.get_raw_id();
                let query = NetQuery::new(
                    id,
                    dc.dc_id,
                    NetQueryType::Common,
                    AuthFlag::Off,
                    GzipFlag::Off,
                    telegram_api::auth_importAuthorization::ID,
                    body,
                    u32::MAX,
                    60 * 60 * 24,
                );
                dispatcher.dispatch_with_callback(
                    query,
                    Box::new(DcAuthCallback {
                        raw_dc_id,
                        completed: self.completed.clone(),
                        shared: self.shared.clone(),
                    }),
                );
                dc.wait_id = id;
                dc.state = DcState::BeforeOk;
            }
            DcState::BeforeOk | DcState::Ok => {}
        }
    }

    fn find_dc(&self, raw_dc_id: i32) -> Option<&DcInfo> {
        self.dcs
            .iter()
            .find(|dc| dc.dc_id.get_raw_id() == raw_dc_id)
    }

    fn find_dc_mut(&mut self, raw_dc_id: i32) -> Option<&mut DcInfo> {
        self.dcs
            .iter_mut()
            .find(|dc| dc.dc_id.get_raw_id() == raw_dc_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth_data_shared::{AuthDataSnapshot, NetEventQueue};
    use crate::session_multi_proxy::PublicRsaKeySharedMain;
    use tdmtproto::auth_key::AuthKey;
    use tdnet::net_query::{DcInitSink, SessionSink, TdSink};

    #[derive(Default)]
    struct RecordingSession {
        sent: Mutex<Vec<NetQuery>>,
    }

    struct SessionHandle(Arc<RecordingSession>);

    impl SessionSink for SessionHandle {
        fn send(&mut self, _dc_id: DcId, _query_type: NetQueryType, query: NetQuery) {
            self.0.sent.lock().push(query);
        }
    }

    struct NullTd;

    impl TdSink for NullTd {
        fn on_result(&mut self, _query: NetQuery) {}
    }

    struct AcceptingInit;

    impl DcInitSink for AcceptingInit {
        fn init_dc(&mut self, _dc_id: DcId) -> bool {
            true
        }

        fn update_main_dc(&mut self, _new_main_dc_id: i32) {}
    }

    /// An authorized main DC: a non-empty key with the auth flag set.
    fn ok_key() -> AuthKey {
        let mut auth_key = AuthKey::new(0x1234_5678_9abc_def0, vec![7u8; 256]);
        auth_key.set_auth_flag(true);
        auth_key
    }

    fn shared_auth(
        shared: &Arc<NetShared>,
        raw_dc_id: i32,
        auth_key: AuthKey,
    ) -> Arc<AuthDataShared> {
        AuthDataShared::new(
            DcId::internal(raw_dc_id),
            AuthDataSnapshot {
                auth_key,
                future_salts: Vec::new(),
                server_time_difference: 0.0,
            },
            shared.events.clone(),
        )
    }

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
            redispatch: Mutex::new(None),
        })
    }

    fn dispatcher_with_sink() -> (Arc<RecordingSession>, NetQueryDispatcher) {
        let session = Arc::new(RecordingSession::default());
        let dispatcher = NetQueryDispatcher::new(
            Box::new(SessionHandle(session.clone())),
            Box::new(NullTd),
            Box::new(AcceptingInit),
        );
        (session, dispatcher)
    }

    fn take_sent(session: &Arc<RecordingSession>) -> Vec<NetQuery> {
        std::mem::take(&mut *session.sent.lock())
    }

    fn complete_ok(query: NetQuery, answer: Vec<u8>) -> bool {
        let mut query = query;
        let Some(mut callback) = query.move_callback() else {
            return false;
        };
        query.set_ok(answer);
        callback.on_result(query);
        true
    }

    fn exported_answer(id: i64, bytes: Vec<u8>) -> Vec<u8> {
        let mut out = Vec::new();
        telegram_api::auth_ExportedAuthorization { id, bytes }.store_boxed(&mut out);
        out
    }

    #[test]
    fn export_rides_the_main_dc_and_import_the_target_dc() {
        let shared = test_shared();
        let (session, dispatcher) = dispatcher_with_sink();
        dispatcher.mark_dc_inited(1);
        dispatcher.mark_dc_inited(2);

        let mut manager = DcAuthManager::new(shared.clone(), DcCompletedQueue::new());
        manager.add_dc(shared_auth(&shared, 1, ok_key()));
        manager.add_dc(shared_auth(&shared, 2, AuthKey::default()));

        // First pass: the export to the main DC.
        manager.loop_(&dispatcher, || dispatcher.next_query_id());
        let sent = take_sent(&session);
        assert_eq!(sent.len(), 1, "one export dispatch");
        assert_eq!(sent[0].dc_id(), DcId::main());
        assert_eq!(
            sent[0].tl_constructor(),
            telegram_api::auth_exportAuthorization::ID
        );
        let export_query_id = sent[0].id();

        // The export answer: the import to the target DC, unauthorized.
        let export_query = sent.into_iter().next().expect("one export dispatch");
        assert!(complete_ok(
            export_query,
            exported_answer(77, vec![1, 2, 3])
        ));
        for (raw_dc_id, query) in manager.take_completed() {
            manager.on_result(raw_dc_id, query);
        }
        manager.loop_(&dispatcher, || dispatcher.next_query_id());
        let sent = take_sent(&session);
        assert_eq!(sent.len(), 1, "one import dispatch");
        assert_eq!(sent[0].dc_id(), DcId::internal(2));
        assert_eq!(
            sent[0].tl_constructor(),
            telegram_api::auth_importAuthorization::ID
        );
        assert_eq!(sent[0].auth_flag(), AuthFlag::Off);

        // A failed import restarts the ladder from a fresh export.
        let first_export_id = export_query_id;
        let mut import_query = sent.into_iter().next().expect("one import dispatch");
        let mut callback = import_query.move_callback().expect("parked callback");
        let mut failed = import_query;
        failed.set_error(tdutils::status::Status::error(400, "AUTH_KEY_UNREGISTERED"));
        callback.on_result(failed);
        for (raw_dc_id, query) in manager.take_completed() {
            manager.on_result(raw_dc_id, query);
        }
        manager.loop_(&dispatcher, || dispatcher.next_query_id());
        let sent = take_sent(&session);
        assert_eq!(sent.len(), 1, "the export restarted");
        assert_eq!(
            sent[0].tl_constructor(),
            telegram_api::auth_exportAuthorization::ID
        );
        assert_ne!(
            sent[0].id(),
            first_export_id,
            "a fresh query id, not the stale one"
        );
    }

    #[test]
    fn keyless_main_dc_blocks_the_ladder_and_fires_the_check() {
        let shared = test_shared();
        let (session, dispatcher) = dispatcher_with_sink();
        dispatcher.mark_dc_inited(1);

        let mut manager = DcAuthManager::new(shared.clone(), DcCompletedQueue::new());
        manager.add_dc(shared_auth(&shared, 1, AuthKey::default()));
        manager.check_authorization_is_ok();

        manager.loop_(&dispatcher, || dispatcher.next_query_id());
        assert!(
            take_sent(&session).is_empty(),
            "no export without a main key"
        );

        // The add-time listener ping (AuthKeyChanged) plus the failed
        // authorization check.
        let events = shared.events.take();
        assert!(matches!(
            events.as_slice(),
            [NetEvent::AuthKeyChanged { raw_dc_id: 1 }, NetEvent::Unauthorized { message }]
                if message == "Authorization check failed in DcAuthManager"
        ));

        // The check is consumed: the second pass stays silent.
        manager.loop_(&dispatcher, || dispatcher.next_query_id());
        let events = shared.events.take();
        assert!(events.is_empty(), "the check must not re-fire: {events:?}");
    }

    #[test]
    fn stale_export_answer_is_ignored() {
        let shared = test_shared();
        let (session, dispatcher) = dispatcher_with_sink();
        dispatcher.mark_dc_inited(1);
        dispatcher.mark_dc_inited(2);

        let mut manager = DcAuthManager::new(shared.clone(), DcCompletedQueue::new());
        manager.add_dc(shared_auth(&shared, 1, ok_key()));
        manager.add_dc(shared_auth(&shared, 2, AuthKey::default()));

        manager.loop_(&dispatcher, || dispatcher.next_query_id());
        let sent = take_sent(&session);
        assert_eq!(sent.len(), 1);
        let export_query = sent.into_iter().next().expect("one export dispatch");

        // An answer under another query id must not advance the machine:
        // the dc stays in Import and the next loop dispatches nothing.
        let mismatched_id = dispatcher.next_query_id();
        let mut mismatched_body = Vec::new();
        telegram_api::auth_exportAuthorization { dc_id: 2 }.store_boxed(&mut mismatched_body);
        let mut mismatched = NetQuery::new(
            mismatched_id,
            DcId::main(),
            NetQueryType::Common,
            AuthFlag::On,
            GzipFlag::Off,
            telegram_api::auth_exportAuthorization::ID,
            mismatched_body,
            u32::MAX,
            60,
        );
        mismatched.set_ok(exported_answer(99, vec![9]));
        manager.on_result(2, mismatched);

        manager.loop_(&dispatcher, || dispatcher.next_query_id());
        assert!(
            take_sent(&session).is_empty(),
            "an unowned answer must not unblock the waiting dc"
        );

        // The real answer still lands: the import follows.
        assert!(complete_ok(
            export_query,
            exported_answer(77, vec![1, 2, 3])
        ));
        for (raw_dc_id, query) in manager.take_completed() {
            manager.on_result(raw_dc_id, query);
        }
        manager.loop_(&dispatcher, || dispatcher.next_query_id());
        let sent = take_sent(&session);
        assert_eq!(sent.len(), 1, "the import followed the real answer");
        assert_eq!(
            sent[0].tl_constructor(),
            telegram_api::auth_importAuthorization::ID
        );
    }
}
