//! Port of the ConfigManager's dispatch layer (M.3.8.1): the four
//! `G()->net_query_dispatcher().dispatch_with_callback(...)` sites
//! (`ConfigManager.cpp:1027, :957, :986, :1002`) and the
//! `Td.cpp:914-919` dispatcher creation, over the Track-F
//! [`NetQueryDispatcher`] slice.
//!
//! The C++ `NetQueryCreator::create/create_unauth` (NetQueryCreator.cpp:42-104)
//! is mirrored by [`build_config_net_query`]: the serialized TL body with the
//! constructor id, the `DcId::main()` default of all four sites, the unauth /
//! auth split, the gzip threshold, and the 60-second (8 for bots) creator
//! default that the config sites overwrite with the 24-hour budget.
//!
//! The `actor_shared(this, N)` payloads — the link tokens the C++
//! `ConfigManager::on_result` dispatches on via `get_link_token()` — ride
//! with [`ConfigQueryCallback`]. M.3.8.2 routes the parked completions into
//! `Td::on_config_query_result`; until then they accumulate in dispatch
//! order in the shared buffers.

use parking_lot::Mutex;
use std::sync::Arc;

use tdnet::net_query::{
    AuthFlag, DcId, GzipFlag, NetQuery, NetQueryCallback, NetQueryType, TdSink,
};

use crate::config_manager::ConfigQuery;
use crate::session_multi_proxy::NetShared;

/// Queries that passed the dispatcher's DC-init gate, parked with their
/// resolved DC by [`TdSessionSink`]. Test-only: production routes
/// through the net-step pool router.
#[cfg(test)]
pub(crate) type SentNetQueries = Arc<Mutex<Vec<(DcId, NetQuery)>>>;

/// Completed ConfigManager queries parked with their link tokens by
/// [`ConfigQueryCallback`] (token 0 = the callback-less [`TdConfigSink`]
/// stand-in).
pub(crate) type CompletedNetQueries = Arc<Mutex<Vec<(u64, NetQuery)>>>;

/// The C++ link token of a dispatched ConfigManager query — the
/// `actor_shared(this, N)` payload the ConfigManager's `on_result`
/// dispatches on (`get_link_token()`): 8/9 = `help.getConfig` (9 = with
/// reopen-sessions), 1 = `help.getAppConfig`, 2 =
/// `account.getContentSettings`, 3/4 = `account.setContentSettings` (4 =
/// enabling the sensitive flag).
pub(crate) fn config_query_token(query: &ConfigQuery) -> u64 {
    match query {
        ConfigQuery::GetConfig {
            reopen_sessions, ..
        } => 8 + u64::from(*reopen_sessions),
        ConfigQuery::GetAppConfig { .. } => 1,
        ConfigQuery::GetContentSettings => 2,
        ConfigQuery::SetContentSettings { sensitive_enabled } => 3 + u64::from(*sensitive_enabled),
    }
}

/// `NetQueryCreator::create`/`create_unauth` for one ConfigManager query —
/// the exact fields the four dispatch sites produce. Returns the query with
/// its link token; the caller attaches the callback
/// (`dispatch_with_callback`) exactly like the C++.
pub(crate) fn build_config_net_query(
    query: &ConfigQuery,
    id: u64,
    is_bot: bool,
) -> (NetQuery, u64) {
    let bytes = query.serialize();
    let tl_constructor = i32::from_le_bytes(bytes[0..4].try_into().unwrap());
    // The auth split of the sites: `create_unauth` for the config reads,
    // `create` for the authorized content-settings pair.
    let auth_flag = match query {
        ConfigQuery::GetConfig { .. } | ConfigQuery::GetAppConfig { .. } => AuthFlag::Off,
        ConfigQuery::GetContentSettings | ConfigQuery::SetContentSettings { .. } => AuthFlag::On,
    };
    // The creator default is 60 (8 for bots, NetQueryCreator.cpp:71-76);
    // the two config sites overwrite it with the 24-hour budget
    // (`query->total_timeout_limit_ = 60 * 60 * 24`, ConfigManager.cpp:1026/:954),
    // the content-settings pair keeps the creator value.
    let total_timeout_limit = match query {
        ConfigQuery::GetConfig { .. } | ConfigQuery::GetAppConfig { .. } => 60 * 60 * 24,
        ConfigQuery::GetContentSettings | ConfigQuery::SetContentSettings { .. } => {
            if is_bot {
                8
            } else {
                60
            }
        }
    };
    // NetQueryCreator.cpp:79 — gzip only when the serialized body crosses
    // the (bot-dependent) threshold; all four bodies are far below it.
    let min_gzipped_size = if is_bot { 1024 } else { 128 };
    let gzip_flag = if bytes.len() < min_gzipped_size {
        GzipFlag::Off
    } else {
        GzipFlag::On
    };
    let net_query = NetQuery::new(
        id,
        DcId::main(), // the dc default of all four sites
        NetQueryType::Common,
        auth_flag,
        gzip_flag,
        tl_constructor,
        bytes,
        // The C++ constructor leaves `dispatch_ttl_` at its -1 member
        // default (never expires under the dispatcher's decrements);
        // `u32::MAX` is the closest `u32`.
        u32::MAX,
        total_timeout_limit,
    );
    (net_query, config_query_token(query))
}

/// The `actor_shared(this, token)` receiver — the ConfigManager side of
/// `dispatch_with_callback`. Its `on_result` parks the completed query with
/// the token and nudges the `Td` actor (the C++ callback's
/// `send_closure(G()->td(), ...)`); M.3.8.2 routes the parked deliveries
/// into `Td::on_config_query_result` with the C++ `on_result` token
/// dispatch.
pub(crate) struct ConfigQueryCallback {
    token: u64,
    completed: CompletedNetQueries,
    /// The actor wake — `None` in the net-inert tests.
    wake: Option<Arc<NetShared>>,
}

impl ConfigQueryCallback {
    /// The test form: no actor wake (the tests drain manually).
    #[cfg(test)]
    pub(crate) fn new(token: u64, completed: CompletedNetQueries) -> Self {
        Self {
            token,
            completed,
            wake: None,
        }
    }

    /// The production form: the completion also wakes the `Td` actor to
    /// drain the parked completions.
    pub(crate) fn with_wake(
        token: u64,
        completed: CompletedNetQueries,
        wake: Option<Arc<NetShared>>,
    ) -> Self {
        Self {
            token,
            completed,
            wake,
        }
    }
}

impl NetQueryCallback for ConfigQueryCallback {
    fn on_result(&mut self, query: NetQuery) {
        self.completed.lock().push((self.token, query));
        if let Some(wake) = &self.wake {
            wake.wake_td();
        }
    }
}

/// The session-routing sink the config tests use — the same
/// [`SessionSink`](tdnet::net_query::SessionSink) plug-in the net step's
/// production router ([`crate::session_multi_proxy::RouterSink`]) fills.
/// The tests accept every DC init, so dispatched queries land here with
/// their resolved DC instead of reaching real sessions.
#[cfg(test)]
pub(crate) struct TdSessionSink {
    pub(crate) sent: SentNetQueries,
}

#[cfg(test)]
impl tdnet::net_query::SessionSink for TdSessionSink {
    fn send(&mut self, dc_id: DcId, _query_type: NetQueryType, query: NetQuery) {
        self.sent.lock().push((dc_id, query));
    }
}

/// The `Td::on_result` plug-in for callback-less queries. No config-path
/// query lands here (they all carry callbacks); the stand-in parks the
/// delivery with the token-0 sentinel for inspection.
pub(crate) struct TdConfigSink {
    pub(crate) completed: CompletedNetQueries,
}

impl TdSink for TdConfigSink {
    fn on_result(&mut self, query: NetQuery) {
        self.completed.lock().push((0, query));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tdmtproto::telegram_api;
    use tdnet::net_query::{DcInitSink, NetQueryDispatcher};

    /// The DC-init stand-in: accepts every init so the dispatched config
    /// queries pass the gate into the [`TdSessionSink`] buffer.
    struct AcceptingDcInit;

    impl DcInitSink for AcceptingDcInit {
        fn init_dc(&mut self, _dc_id: DcId) -> bool {
            true
        }

        fn update_main_dc(&mut self, _new_main_dc_id: i32) {}
    }

    fn dispatcher_with_sinks() -> (SentNetQueries, CompletedNetQueries, NetQueryDispatcher) {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let completed = Arc::new(Mutex::new(Vec::new()));
        let dispatcher = NetQueryDispatcher::new(
            Box::new(TdSessionSink { sent: sent.clone() }),
            Box::new(TdConfigSink {
                completed: completed.clone(),
            }),
            Box::new(AcceptingDcInit),
        );
        (sent, completed, dispatcher)
    }

    fn dispatch(
        dispatcher: &NetQueryDispatcher,
        completed: &CompletedNetQueries,
        query: &ConfigQuery,
        is_bot: bool,
    ) -> u64 {
        let (net_query, token) = build_config_net_query(query, dispatcher.next_query_id(), is_bot);
        let callback = ConfigQueryCallback::new(token, completed.clone());
        dispatcher.dispatch_with_callback(net_query, Box::new(callback));
        token
    }

    /// The construction pins of all four query kinds: the serialized body,
    /// the constructor id, the auth split, the `DcId::main()` default,
    /// gzip off and the 24-hour / creator-default timeout budgets with the
    /// C++ link tokens.
    #[test]
    fn construction_pins_all_four_kinds() {
        let cases: Vec<(ConfigQuery, i32, AuthFlag, i32, u64)> = vec![
            (
                ConfigQuery::GetConfig {
                    dc_id: 1,
                    reopen_sessions: false,
                },
                telegram_api::help_getConfig::ID,
                AuthFlag::Off,
                60 * 60 * 24,
                8,
            ),
            (
                ConfigQuery::GetConfig {
                    dc_id: 1,
                    reopen_sessions: true,
                },
                telegram_api::help_getConfig::ID,
                AuthFlag::Off,
                60 * 60 * 24,
                9,
            ),
            (
                ConfigQuery::GetAppConfig { hash: -12345 },
                telegram_api::help_getAppConfig::ID,
                AuthFlag::Off,
                60 * 60 * 24,
                1,
            ),
            (
                ConfigQuery::GetContentSettings,
                telegram_api::account_getContentSettings::ID,
                AuthFlag::On,
                60,
                2,
            ),
            (
                ConfigQuery::SetContentSettings {
                    sensitive_enabled: false,
                },
                telegram_api::account_setContentSettings::ID,
                AuthFlag::On,
                60,
                3,
            ),
            (
                ConfigQuery::SetContentSettings {
                    sensitive_enabled: true,
                },
                telegram_api::account_setContentSettings::ID,
                AuthFlag::On,
                60,
                4,
            ),
        ];
        for (query, tl_constructor, auth_flag, total_timeout_limit, token) in cases {
            let (net_query, got_token) = build_config_net_query(&query, 77, false);
            assert_eq!(got_token, token, "token of {query:?}");
            assert_eq!(net_query.id(), 77);
            assert!(net_query.dc_id().is_main(), "the dc default of the sites");
            assert_eq!(net_query.query_type(), NetQueryType::Common);
            assert_eq!(net_query.auth_flag(), auth_flag, "auth of {query:?}");
            assert_eq!(net_query.gzip_flag(), GzipFlag::Off, "tiny bodies");
            assert_eq!(net_query.tl_constructor(), tl_constructor);
            assert_eq!(net_query.query(), query.serialize().as_slice());
            assert_eq!(net_query.total_timeout_limit(), total_timeout_limit);
        }

        // The bot creator default applies to the content-settings pair.
        for query in [
            ConfigQuery::GetContentSettings,
            ConfigQuery::SetContentSettings {
                sensitive_enabled: true,
            },
        ] {
            let (net_query, _) = build_config_net_query(&query, 1, true);
            assert_eq!(
                net_query.total_timeout_limit(),
                8,
                "bot budget of {query:?}"
            );
        }
        // The config pair keeps the 24-hour budget regardless (the C++
        // overwrites the creator value after `create_unauth`).
        let (net_query, _) = build_config_net_query(
            &ConfigQuery::GetConfig {
                dc_id: 1,
                reopen_sessions: false,
            },
            1,
            true,
        );
        assert_eq!(net_query.total_timeout_limit(), 60 * 60 * 24);
    }

    /// A dispatched query against a session-inited DC reaches the session
    /// sink with the resolved main DC, and its callback delivery carries
    /// the reopen-sessions token — the end-to-end 8/9 pin.
    #[test]
    fn dispatch_reaches_session_sink_with_token() {
        let (sent, completed, dispatcher) = dispatcher_with_sinks();
        dispatcher.mark_dc_inited(1);

        for (reopen_sessions, token) in [(false, 8u64), (true, 9u64)] {
            dispatch(
                &dispatcher,
                &completed,
                &ConfigQuery::GetConfig {
                    dc_id: 1,
                    reopen_sessions,
                },
                false,
            );

            {
                let mut sent = sent.lock();
                assert_eq!(sent.len(), 1, "one parked query");
                let (dest_dc, mut query) = sent.pop().unwrap();
                assert_eq!(dest_dc, DcId::internal(1), "main resolved to the default 1");
                assert_eq!(query.total_timeout_limit(), 60 * 60 * 24, "the 24 h budget");
                assert_eq!(query.tl_constructor(), telegram_api::help_getConfig::ID);
                // The attached callback is the token carrier: deliver the
                // query back through it.
                assert!(completed.lock().is_empty(), "nothing completed yet");
                let mut callback = query.move_callback().expect("dispatch_with_callback");
                callback.on_result(query);
            }
            let (got_token, _) = completed
                .lock()
                .pop()
                .expect("the callback delivery parked the query");
            assert_eq!(got_token, token, "reopen-sessions token");
            assert!(completed.lock().is_empty(), "one per dispatch");
        }
    }

    /// The DC-init stand-in that refuses every init — the dispatcher's
    /// gate then completes each query with the C++ "No such dc" error.
    struct RefusingDcInit;

    impl DcInitSink for RefusingDcInit {
        fn init_dc(&mut self, _dc_id: DcId) -> bool {
            false
        }

        fn update_main_dc(&mut self, _new_main_dc_id: i32) {}
    }

    #[test]
    fn dc_gate_completions_keep_dispatch_order() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let completed = Arc::new(Mutex::new(Vec::new()));
        let dispatcher = NetQueryDispatcher::new(
            Box::new(TdSessionSink { sent: sent.clone() }),
            Box::new(TdConfigSink {
                completed: completed.clone(),
            }),
            Box::new(RefusingDcInit),
        );

        let first = dispatch(
            &dispatcher,
            &completed,
            &ConfigQuery::GetConfig {
                dc_id: 1,
                reopen_sessions: false,
            },
            false,
        );
        let second = dispatch(
            &dispatcher,
            &completed,
            &ConfigQuery::GetAppConfig { hash: 7 },
            false,
        );
        assert_eq!((first, second), (8, 1));

        assert!(sent.lock().is_empty(), "nothing reached a session");
        let completed = completed.lock();
        assert_eq!(completed.len(), 2);
        assert_eq!(completed[0].0, 8);
        assert_eq!(
            completed[0].1.error().message(),
            "No such dc DcId{1}",
            "the DC-init gate error"
        );
        assert_eq!(completed[1].0, 1);
        assert_eq!(completed[1].1.error().message(), "No such dc DcId{1}");
    }
}
