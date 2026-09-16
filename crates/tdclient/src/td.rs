//! Port of `td/telegram/Td.{h,cpp}` (Track I, Td M.1): the full request
//! state machine — `WaitParameters → Run → Close` — the `setTdlibParameters`
//! validation (`Td::get_parameters`), the in-process `TdDb::open` + init
//! sequence (options pmc replay, AuthManager restore), the request tracking
//! (`request_set_`), the `send_result`/`send_error` semantics and the
//! close/destroy machine.
//!
//! Divergences (each arrives with its Wave-1 step):
//! - the database opens synchronously in the actor; the C++ hops to a
//!   database scheduler (the schedulers arrive with the net wiring step);
//! - `init` creates only the managers that exist (options, config, auth);
//!   every manager step appends its wiring here, mirroring the C++ `Td::init`
//!   order;
//! - the close machine collapses the actor-refcount ladder (2→3→4→5) into
//!   direct calls — there are no manager actors to await yet;
//! - the ConfigManager is an inline manager: its network queries dispatch
//!   through the client-side NetQueryDispatcher (M.3.8.1) with the parked
//!   completions awaiting M.3.8.2's routing, and its expire timer rides
//!   the Td's own actor timeout slot; the ConfigRecoverer rides with it —
//!   created at the manager's `start_up`, its dispatched fetches parked
//!   until their executors, its wakeup sharing the same timeout slot.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use tdactor::{Actor, ActorOwn, actor_messages};
use tddb::td_db::{OpenedDatabase, Parameters as DbParameters, TdDb};
use tdmtproto::telegram_api;
use tdnet::net_query::{NetQuery, NetQueryDispatcher};
use tdutils::status::Status;

use crate::auth_manager::AuthManager;
use crate::config_dispatch::{
    CompletedNetQueries, ConfigQueryCallback, SentNetQueries, TdConfigSink, TdSessionSink,
    build_config_net_query,
};
use crate::config_manager::{
    ConfigActions, ConfigManager, ConfigQuery, ConfigQueryResult, StoredDcOption,
};
use crate::config_recoverer::{
    ConfigRecoverer, RecovererActions, RecovererConnectionState, RecovererDcOption, RecovererEnv,
    RecovererNetType, SimpleConfigResult, load_dc_options_update,
};
use crate::config_result;
use crate::connection_creator::ConnectionCreator;
use crate::global::Global;
use crate::link_manager::LinkManager;
use crate::option_manager::OptionManager;
use crate::requests;
use crate::synchronous_requests;
use crate::td_api::{self, Function, Object};
use crate::time_zone_manager::TimeZoneManager;

#[cfg(target_endian = "big")]
compile_error!("TDLib requires little-endian platform");

/// `td::TdCallback` — the response channel from the Td actor back to the
/// ClientManager. The receiver-side callback's destructor sends the
/// `{client_id, 0, nullptr}` close notification (see `client.rs`).
pub trait TdCallback: Send {
    fn on_result(&mut self, id: u64, result: Object);
    fn on_error(&mut self, id: u64, error: td_api::Error);
}

/// `Td::State` (Td.h:400).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    WaitParameters,
    Run,
    Close,
}

/// `Td::Parameters` — the non-database initialization parameters.
pub struct Parameters {
    pub api_id: i32,
    pub api_hash: String,
    pub use_secret_chats: bool,
}

/// `MtprotoHeader::Options` — gathered by `get_parameters`/`init`; consumed
/// when the MTProto header wiring lands with the net step.
#[derive(Clone, Debug, Default)]
pub struct HeaderOptions {
    pub api_id: i32,
    pub system_language_code: String,
    pub device_model: String,
    pub system_version: String,
    pub application_version: String,
    pub language_pack: String,
    pub language_code: String,
    pub parameters: String,
    pub tz_offset: i32,
    pub is_emulator: bool,
}

/// The full Td actor: the request entry point and the manager orchestrator.
pub struct Td {
    callback: Option<Box<dyn TdCallback>>,
    pub(crate) state: State,
    pub(crate) close_flag: u8,
    pub(crate) destroy_flag: bool,
    /// `Td::request_set_` — ids of in-flight requests (the C++ value is the
    /// constructor id, used only for the lost-promise diagnostics).
    request_set: BTreeSet<u64>,
    pub(crate) set_parameters_request_id: u64,
    pub(crate) pending_set_parameters_requests: Vec<(u64, Function)>,
    /// `Td::pending_preauthentication_requests_`; the C++ nulls entries as
    /// they are replayed — the Rust keeps the original and filters by id.
    pending_preauthentication_requests: Vec<(u64, Function)>,
    pub(crate) can_ignore_background_updates: bool,
    parameters: Option<Parameters>,
    pub(crate) header_options: HeaderOptions,
    /// `Td::option_manager_` — created by `init` over the db config pmc;
    /// `None` during `WaitParameters` exactly like the C++.
    pub(crate) option_manager: Option<OptionManager>,
    /// The C++ `Td::connection_creator_` (`Td.cpp:1049`) — the M.3.9.1
    /// dc-options consumer slice; the connection machinery (the
    /// `DcOptionsSet` orderings, the link loop) is the net step.
    pub(crate) connection_creator: Option<ConnectionCreator>,
    /// The C++ `Td::config_manager_` — created by `init_options_and_network`; its
    /// expire timer rides the Td's actor timeout slot and its queries
    /// dispatch through the client-side dispatcher (M.3.8.1).
    pub(crate) config_manager: Option<ConfigManager>,
    /// The client-side `NetQueryDispatcher` (`Td.cpp:914-919`), created in
    /// `init`; the ConfigManager queries and the `pingProxy` main-DC read
    /// go through it.
    pub(crate) net_query_dispatcher: Option<NetQueryDispatcher>,
    /// Queries that passed the dispatcher's DC-init gate, parked with
    /// their resolved DC by `TdSessionSink` until the Track-G session step
    /// routes them to real sessions.
    pub(crate) sent_net_queries: SentNetQueries,
    /// Completed ConfigManager queries — `(token, NetQuery)` parked by
    /// `ConfigQueryCallback` (token 0 = the callback-less `TdSink`
    /// stand-in); M.3.8.2 routes them into `on_config_query_result`.
    pub(crate) completed_config_queries: CompletedNetQueries,
    /// The C++ `ConfigManager::config_recoverer_` — created right after
    /// the ConfigManager's `start_up` (ConfigManager.cpp:866-868) and
    /// dropped with it at `ConfigManager::hangup` (ConfigManager.cpp:894).
    pub(crate) config_recoverer: Option<ConfigRecoverer>,
    /// The recoverer's dispatched fetches, parked until their executors
    /// land: the async dispatch needs a runtime on the actor thread, which
    /// arrives with the net step (results come back through the
    /// `RecovererSimpleConfigResult`/`RecovererFullConfigResult` messages,
    /// which tests and the executors alike deliver).
    pub(crate) pending_recoverer_fetches: Vec<RecovererFetch>,
    /// The recoverer's armed wakeup (`set_timeout_at` source, sharing the
    /// single Td timeout slot with the config expire time — the earliest
    /// wins, and the fired timeout re-loops both state machines).
    recoverer_wakeup_at: f64,
    /// Tests pin the recoverer's clocks here (the production driver reads
    /// the process clock; the loop's gates are real-time seconds).
    #[cfg(test)]
    recoverer_clock: Option<(f64, f64)>,
    /// The `G()` shared context; owns the database (`Global::td_db_`).
    pub(crate) global: Global,
    /// The C++ `init_managers` registration order starts here; managers
    /// accumulate in their C++ positions as their steps land. The first
    /// consumer (PhoneNumberManager's `getPhoneNumberInfo`) reads it via
    /// `G()->time_zone_manager()`.
    #[allow(dead_code)]
    pub(crate) time_zone_manager: TimeZoneManager,
    /// The C++ `Td::link_manager_` (`make_unique` at `Td.cpp:1239`, the
    /// `init_managers` block) — the M.3.9.1 autologin-token slice; the
    /// link surface is Wave 2.
    pub(crate) link_manager: Option<LinkManager>,
    pub(crate) auth_manager: AuthManager,
    /// `Td::stop_cnt_` — the hangup and the close completion each drop one.
    stop_cnt: i32,
}

/// A parked recoverer fetch — the C++ `get_simple_config_*` /
/// `get_full_config` actor handles the Td holds until the executors' async
/// dispatch lands with the net step (the executors themselves are
/// [`crate::config_recoverer::fetch_simple_config`] and
/// [`crate::config_recoverer::fetch_full_config`]; the M.3.7.4
/// mini-session's spec rides the full arm).
#[allow(dead_code)]
pub(crate) enum RecovererFetch {
    Simple(crate::config_recoverer::SimpleConfigFetch),
    Full(crate::config_recoverer::FullConfigFetch),
}

impl Td {
    pub fn new(callback: Box<dyn TdCallback>) -> Self {
        Self {
            callback: Some(callback),
            state: State::WaitParameters,
            close_flag: 0,
            destroy_flag: false,
            request_set: BTreeSet::new(),
            set_parameters_request_id: 0,
            pending_set_parameters_requests: Vec::new(),
            pending_preauthentication_requests: Vec::new(),
            can_ignore_background_updates: false,
            parameters: None,
            header_options: HeaderOptions::default(),
            option_manager: None,
            connection_creator: None,
            config_manager: None,
            net_query_dispatcher: None,
            sent_net_queries: Arc::new(Mutex::new(Vec::new())),
            completed_config_queries: Arc::new(Mutex::new(Vec::new())),
            config_recoverer: None,
            pending_recoverer_fetches: Vec::new(),
            recoverer_wakeup_at: 0.0,
            #[cfg(test)]
            recoverer_clock: None,
            global: Global::new(),
            time_zone_manager: TimeZoneManager::default(),
            link_manager: None,
            auth_manager: AuthManager::new(),
            stop_cnt: 2,
        }
    }

    /// `Td::request` (Td.cpp:279-297): track the id, answer the synchronous
    /// set immediately, everything else through `run_request`.
    fn request(&mut self, request_id: u64, function: Function) {
        if request_id == 0 {
            tracing::error!("Ignore request with ID == 0");
            return;
        }

        self.request_set.insert(request_id);
        if synchronous_requests::is_synchronous_request(&function) {
            // send response synchronously
            return self.send_result(request_id, synchronous_requests::run_request(function));
        }

        self.run_request(request_id, function);
    }

    /// `Td::run_request` (Td.cpp:299-374) — the state gate.
    fn run_request(&mut self, id: u64, function: Function) {
        if self.set_parameters_request_id > 0 {
            self.pending_set_parameters_requests.push((id, function));
            return;
        }

        if self.state != State::Run {
            match &function {
                Function::getAuthorizationState(_) => {
                    // send response synchronously to prevent "Request aborted"
                    return self.send_result(id, self.fake_authorization_state_object());
                }
                Function::getCurrentState(_) => {
                    // send response synchronously to prevent "Request aborted"
                    let updates: Vec<td_api::Update> = self
                        .get_fake_current_state()
                        .into_iter()
                        .map(|update| {
                            td_api::Update::try_from_object(update).expect("fake state is updates")
                        })
                        .collect();
                    return self.send_result(id, Object::updates(td_api::Updates { updates }));
                }
                Function::close(_) => {
                    // need to send response before actual closing
                    self.send_result(id, ok_object());
                    return self.close_impl(false);
                }
                _ => {}
            }
        }
        match self.state {
            State::WaitParameters => match function {
                Function::setTdlibParameters(p) => self.on_set_tdlib_parameters(id, p),
                ref function if is_preinitialization_request(function) => {
                    requests::run_request(self, id, function.clone());
                }
                ref function if is_preauthentication_request(function) => {
                    self.pending_preauthentication_requests
                        .push((id, function.clone()));
                }
                _ => {
                    self.send_error_impl(
                        id,
                        400,
                        "Initialization parameters are needed: call setTdlibParameters first",
                    );
                }
            },
            State::Close => {
                let (code, message) = if self.destroy_flag {
                    (401, "Unauthorized")
                } else {
                    (500, "Request aborted")
                };
                self.send_error_impl(id, code, message);
            }
            State::Run => {
                if !self.auth_manager.is_authorized()
                    && !is_preauthentication_request(&function)
                    && !is_preinitialization_request(&function)
                    && !is_authentication_request(&function)
                {
                    return self.send_error_impl(id, 401, "Unauthorized");
                }
                requests::run_request(self, id, function);
            }
        }
    }

    /// The `setTdlibParameters` arm of `Td::run_request` (Td.cpp:326-348).
    /// The C++ opens the database on the database scheduler and continues in
    /// `Td::init`; the Rust opens it in-process (divergence until the
    /// database scheduler lands).
    fn on_set_tdlib_parameters(&mut self, id: u64, p: td_api::setTdlibParameters) {
        let r_parameters = self.get_parameters(p);
        let (parameters, db_parameters) = match r_parameters {
            Ok(ok) => ok,
            Err(error) => {
                return self.send_error_impl(id, error.code, &error.message);
            }
        };

        tracing::info!("Begin to open database");
        self.set_parameters_request_id = id;
        self.can_ignore_background_updates = !db_parameters.use_chat_info_database
            && !db_parameters.use_message_database
            && !parameters.use_secret_chats;

        let opened = TdDb::open(db_parameters);
        self.init(parameters, opened);
    }

    /// `Td::init` (Td.cpp:835-962), the M.1 subset in the C++ order.
    fn init(
        &mut self,
        parameters: Parameters,
        r_opened_database: tdutils::status::Result<OpenedDatabase>,
    ) {
        let mut opened_database = match r_opened_database {
            tdutils::status::Result::Ok(ok) => ok,
            tdutils::status::Result::Err(error) => {
                tracing::warn!("Failed to open database: {error}");
                let request_id = self.set_parameters_request_id;
                self.send_error_impl(request_id, error.code(), error.message());
                return self.finish_set_parameters();
            }
        };

        tracing::info!("Successfully inited database");
        let since_last_open = opened_database.since_last_open;

        if self.state == State::Close {
            // Close asynchronously opened database
            opened_database.database.close_all(self.destroy_flag);
            return self.finish_set_parameters();
        }

        self.global.init(opened_database.database);

        // init_options_and_network, the M.1 + ConfigManager subset: the
        // OptionManager over the db config pmc, then the ConfigManager
        // (StateManager/OnlineManager arrive with their steps; the
        // ConfigManager's start_up — the post-init config fetch — runs
        // after the auth restore below, like the C++ actor start).
        // `global.init` above already loaded the server time difference, so
        // the constructor's opening `unix_time` update (OptionManager.cpp:59)
        // is corrected.
        {
            let config_pmc = self.global.td_db().take_config_pmc();
            let is_test_dc = self.global.is_test_dc();
            let unix_time = self.global.unix_time();
            let mut option_manager = OptionManager::new(is_test_dc);
            option_manager.attach_pmc(config_pmc, unix_time);
            self.option_manager = Some(option_manager);
        }
        self.flush_option_updates();

        // VLOG "Create ConnectionCreator" (Td.cpp:1049, before the
        // ConfigManager): the `start_up` dc-options pmc replay runs now —
        // an unparsable/empty store routes through `on_dc_options(empty)`,
        // which persists the empty list and re-adds the defaults. The
        // actor machinery around it is the net step.
        let is_test_dc = self.global.is_test_dc();
        self.connection_creator =
            Some(ConnectionCreator::new(self.global.binlog_pmc(), is_test_dc));

        // VLOG "Create ConfigManager" (Td.cpp:1070-1072).
        {
            let app_config = self.global.binlog_pmc().get_raw(b"app_config");
            self.config_manager = Some(ConfigManager::new(&app_config));
        }

        // we need to process td_api::setOption before managers are created,
        // because their initialization may be affected by the options
        self.complete_pending_preauthentication_requests(&[
            FunctionKind::GetOption,
            FunctionKind::SetOption,
        ]);

        if since_last_open >= 3600 {
            let option_manager = self
                .option_manager
                .as_mut()
                .expect("option manager just set");
            let old_since_last_open = option_manager.get_option_integer("since_last_open", 0);
            if since_last_open > old_since_last_open {
                option_manager.set_option_integer("since_last_open", since_last_open);
            }
        }

        {
            let option_manager = self
                .option_manager
                .as_ref()
                .expect("option manager just set");
            let header_options = &mut self.header_options;
            header_options.language_pack =
                option_manager.get_option_string("localization_target", "");
            header_options.language_code = option_manager.get_option_string("language_pack_id", "");
            header_options.parameters =
                option_manager.get_option_string("connection_parameters", "");
            header_options.tz_offset =
                option_manager.get_option_integer("utc_time_offset", 0) as i32;
            header_options.is_emulator = option_manager.get_option_boolean("is_emulator", false);
        }

        // VLOG "Create NetQueryDispatcher" (Td.cpp:914-919): the
        // client-side dispatcher over the session/td sinks — the
        // ConfigManager queries dispatch through it below; the pending
        // pingProxy requests complete once their main-DC reads go through
        // it.
        self.net_query_dispatcher = Some(NetQueryDispatcher::new(
            Box::new(TdSessionSink {
                sent: self.sent_net_queries.clone(),
            }),
            Box::new(TdConfigSink {
                completed: self.completed_config_queries.clone(),
            }),
        ));

        // VLOG "Create AuthManager" (Td.cpp:923-926) + the constructor pmc
        // restore (AuthManager.cpp:337-366).
        self.auth_manager
            .set_api_credentials(parameters.api_id, parameters.api_hash.clone());
        {
            let binlog_pmc = self.global.binlog_pmc();
            let auth_str = binlog_pmc.get("auth");
            match auth_str.as_str() {
                "ok" => {
                    let is_bot = binlog_pmc.get("auth_is_bot") == "true";
                    // The C++ checks UserManager::load_my_id() here; the
                    // my_id restore arrives with the UserManager step.
                    self.auth_manager
                        .restore_from_pmc("ok", is_bot, String::new());
                }
                "logout" => {
                    tracing::warn!("Continue to log out");
                    self.auth_manager
                        .restore_from_pmc("logout", false, String::new());
                }
                "destroy" => {
                    tracing::warn!("Continue to destroy auth keys");
                    self.auth_manager
                        .restore_from_pmc("destroy", false, String::new());
                }
                _ => {
                    // `if (!load_state()) update_state(State::WaitPhoneNumber)`
                    let now = tdutils::time::now();
                    let auth_state = binlog_pmc.get("auth_state");
                    let restored = !auth_state.is_empty()
                        && self
                            .auth_manager
                            .load_state(auth_state.as_bytes(), now)
                            .is_ok();
                    if !restored {
                        self.auth_manager.restore_from_pmc("", false, String::new());
                    }
                }
            }
        }
        self.flush_auth_updates();

        // init_file_manager / init_non_actor_managers / init_managers /
        // init_pure_actor_managers / SecretChatsManager / StorageManager —
        // each manager step appends its block here in the C++ order.

        // `make_unique<LinkManager>` (Td.cpp:1239, the init_managers
        // block) — the autologin-token slice (M.3.9.1); the link surface
        // is Wave 2.
        self.link_manager = Some(LinkManager::new());

        // `ConfigManager::start_up` (runs when the C++ manager actor
        // starts, i.e. after the auth restore): the post-init config fetch
        // — a fresh start or a non-bot restart requests `help.getConfig`.
        let start_up_actions = {
            self.config_manager
                .as_mut()
                .expect("config manager just set")
                .start_up(self.global.binlog_pmc(), self.auth_manager.is_bot())
        };
        self.apply_config_actions(start_up_actions);

        // `ConfigManager::start_up` also creates the ConfigRecoverer and
        // replays the saved `dc_options_update` into it
        // (ConfigManager.cpp:866-868). The C++ recoverer actor registers
        // its StateCallback with the StateManager in its own `start_up`
        // (ConfigManager.cpp:799-821); the StateManager lands with the net
        // step, so the callback legs are the Td's `on_recoverer_*`
        // forwarders until then.
        let environment = self.recoverer_env();
        let saved_options = load_dc_options_update(self.global.binlog_pmc());
        let mut recoverer = ConfigRecoverer::new();
        let recoverer_actions = recoverer.on_dc_options_update(saved_options, &environment);
        self.config_recoverer = Some(recoverer);
        self.apply_recoverer_actions(recoverer_actions);

        self.parameters = Some(parameters);

        // `OptionManager::on_td_inited` (Td.cpp:943): the pending pre-init
        // getters are answered now — the content-settings names go through
        // the ConfigManager branch of the C++ `OptionManager::get_option`.
        let pending_get_options = self
            .option_manager
            .as_mut()
            .map(|om| om.on_td_inited())
            .unwrap_or_default();
        for (request_id, name) in pending_get_options {
            if name == "can_ignore_sensitive_content_restrictions"
                || name == "ignore_sensitive_content_restrictions"
            {
                self.get_option_content_settings(request_id, name);
                continue;
            }
            let Some(option_manager) = self.option_manager.as_ref() else {
                break;
            };
            let value = option_manager.get_option_value(&name, self.global.unix_time());
            self.send_result(request_id, requests::option_value_to_object(value));
        }

        // process_binlog_events — the per-manager event vectors arrive with
        // the managers that own them.

        // The init ping: country code pre-auth / get_difference post-auth —
        // both need the net step.

        self.complete_pending_preauthentication_requests(&[FunctionKind::Any]);

        tracing::info!("Finish initialization");

        self.state = State::Run;

        let request_id = self.set_parameters_request_id;
        self.send_result(request_id, ok_object());
        self.finish_set_parameters();
    }

    /// `Td::finish_set_parameters` (Td.cpp:819-833).
    fn finish_set_parameters(&mut self) {
        assert_ne!(self.set_parameters_request_id, 0);
        self.set_parameters_request_id = 0;

        if self.pending_set_parameters_requests.is_empty() {
            return;
        }

        let requests = std::mem::take(&mut self.pending_set_parameters_requests);
        for (id, function) in requests {
            self.run_request(id, function);
        }
    }

    /// `Td::complete_pending_preauthentication_requests`.
    fn complete_pending_preauthentication_requests(&mut self, kinds: &[FunctionKind]) {
        let mut replayed = Vec::new();
        self.pending_preauthentication_requests
            .retain(|(id, function)| {
                if kinds.contains(&FunctionKind::Any) || kinds.contains(&FunctionKind::of(function))
                {
                    replayed.push((*id, function.clone()));
                    false
                } else {
                    true
                }
            });
        for (id, function) in replayed {
            requests::run_request(self, id, function);
        }
    }

    /// `Td::close` / `Td::destroy` / `Td::close_impl` (Td.cpp:768-807).
    pub(crate) fn close_impl(&mut self, destroy_flag: bool) {
        self.destroy_flag |= destroy_flag;
        if self.close_flag != 0 {
            return;
        }

        if self.state == State::WaitParameters {
            self.state = State::Close;
            self.close_flag = 4;
            self.global.set_close_flag();
            self.clear_requests();
            self.send_update(Object::updateAuthorizationState(
                td_api::updateAuthorizationState {
                    authorization_state: td_api::AuthorizationState::authorizationStateClosing(
                        td_api::authorizationStateClosing,
                    ),
                },
            ));
        } else {
            self.state = State::Close;
            self.close_flag = 1;
            self.global.set_close_flag();
            self.auth_manager.on_closing(self.destroy_flag);
            self.flush_auth_updates();
            // updates_manager_->timeout_expired() (save PTS and QTS) and the
            // request-actor drain arrive with their steps.
        }
        // The C++ hops through the request-actor refcount into clear(); the
        // M.1 has no request actors, so the teardown is direct.
        self.clear();
    }

    /// `Td::clear` (Td.cpp:643-766), the M.1 subset: abort requests, close
    /// the database, run the 4→5 transition. The manager-actor resets
    /// accumulate here as the managers land; the C++ early-returns once
    /// `close_flag >= 2`, which cannot re-enter here — `clear` runs exactly
    /// once, from `close_impl`.
    fn clear(&mut self) {
        if self.close_flag < 2 {
            self.close_flag = 2;
        }

        // G()->net_query_creator().stop_check() / result_handlers_ /
        // StateManager — with the net step.
        self.clear_requests();

        // `net_query_dispatcher_->stop()` (the Td.cpp:664 close path): the
        // stop flag aborts every still-dispatched query with
        // Global::request_aborted_error() (NetQueryDispatcher.cpp:49-55).
        // M.3.8.2 close-drop: the DC-gate-passed queries abort through
        // their callbacks into the completions buffer, then every parked
        // completion routes through the token dispatch — failing the
        // manager's pending promise queues (ConfigManager.cpp:1130)
        // before the buffers and the manager drop. The C++ delivers these
        // asynchronously after clear; the collapsed-actor form routes
        // them directly. Client-visible sends no-op — `clear_requests`
        // already ran, like the C++.
        if let Some(dispatcher) = self.net_query_dispatcher.take() {
            dispatcher.stop();
        }
        for (_, mut net_query) in std::mem::take(&mut *self.sent_net_queries.lock().unwrap()) {
            net_query.set_error(config_result::request_aborted_error());
            if let Some(mut callback) = net_query.move_callback() {
                callback.on_result(net_query);
            }
        }
        self.drain_config_net_query_completions();
        self.sent_net_queries.lock().unwrap().clear();
        self.completed_config_queries.lock().unwrap().clear();

        self.global.close_all(self.destroy_flag);
        self.option_manager = None;
        // The C++ close clears the ConnectionCreator early
        // (`G()->set_connection_creator({})`, Td.cpp:690) and the
        // LinkManager with the managers (`reset_manager(link_manager_)`,
        // Td.cpp:547) — the component slices drop with them.
        self.connection_creator = None;
        self.link_manager = None;
        self.config_manager = None;
        // The C++ `ConfigManager::hangup` drops `config_recoverer_`
        // (ConfigManager.cpp:892-896): the close releases the parent share
        // and cancels the in-flight queries with it, then the dropped
        // handle stops the actor — the parked fetches die with it.
        if let Some(mut recoverer) = self.config_recoverer.take() {
            recoverer.hangup();
        }
        self.pending_recoverer_fetches.clear();
        self.recoverer_wakeup_at = 0.0;

        self.close_flag = 4;
        self.on_closed();
    }

    /// `Td::on_closed` (Td.cpp:603-608).
    fn on_closed(&mut self) {
        self.close_flag = 5;
        self.send_update(Object::updateAuthorizationState(
            td_api::updateAuthorizationState {
                authorization_state: td_api::AuthorizationState::authorizationStateClosed(
                    td_api::authorizationStateClosed,
                ),
            },
        ));
        self.dec_stop_cnt();
    }

    /// `Td::dec_stop_cnt` (Td.cpp:610-617): the two decrements are the
    /// hangup and the close completion.
    fn dec_stop_cnt(&mut self) {
        self.stop_cnt -= 1;
        if self.stop_cnt == 0 {
            // The C++ restores the outer actor context here; the Rust
            // scheduler keeps per-actor contexts, nothing to restore.
            tdactor::Actor::stop(self);
        }
    }

    /// `Td::clear_requests` (Td.cpp:632-641).
    fn clear_requests(&mut self) {
        let ids: Vec<u64> = self.request_set.iter().copied().collect();
        for id in ids {
            if self.destroy_flag {
                self.send_error_impl(id, 401, "Unauthorized");
            } else {
                self.send_error_impl(id, 500, "Request aborted");
            }
        }
    }

    /// `Td::get_fake_authorization_state_object` (Td.cpp:252-269).
    fn fake_authorization_state_object(&self) -> Object {
        match self.state {
            State::WaitParameters => Object::authorizationStateWaitTdlibParameters(
                td_api::authorizationStateWaitTdlibParameters,
            ),
            State::Run => unreachable!("get_fake_authorization_state_object in State::Run"),
            State::Close => {
                if self.close_flag == 5 {
                    Object::authorizationStateClosed(td_api::authorizationStateClosed)
                } else {
                    Object::authorizationStateClosing(td_api::authorizationStateClosing)
                }
            }
        }
    }

    /// `Td::get_fake_current_state` (Td.cpp:271-277).
    fn get_fake_current_state(&self) -> Vec<Object> {
        assert_ne!(self.state, State::Run);
        let mut updates = Vec::new();
        OptionManager::get_common_state(&mut updates);
        let authorization_state = match self.fake_authorization_state_object() {
            Object::authorizationStateWaitTdlibParameters(v) => {
                td_api::AuthorizationState::authorizationStateWaitTdlibParameters(v)
            }
            Object::authorizationStateClosing(v) => {
                td_api::AuthorizationState::authorizationStateClosing(v)
            }
            Object::authorizationStateClosed(v) => {
                td_api::AuthorizationState::authorizationStateClosed(v)
            }
            _ => unreachable!("fake authorization state is one of the three states"),
        };
        updates.push(Object::updateAuthorizationState(
            td_api::updateAuthorizationState {
                authorization_state,
            },
        ));
        updates
    }

    /// `Td::send_update` (Td.cpp:1356-1397) — the verbosity logging is a
    /// no-op here; the close gate is kept.
    pub(crate) fn send_update(&mut self, object: Object) {
        if self.close_flag >= 5 {
            // just in case (updateAuthorizationState still passes)
            if !matches!(object, Object::updateAuthorizationState(_)) {
                return;
            }
        }
        let Some(callback) = self.callback.as_mut() else {
            return;
        };
        callback.on_result(0, object);
    }

    /// Drain the inline AuthManager's queued `updateAuthorizationState`
    /// broadcasts (the C++ sends them straight to `Td::send_update`).
    fn flush_auth_updates(&mut self) {
        for update in self.auth_manager.take_updates() {
            self.send_update(update);
        }
    }

    /// Drain the OptionManager's queued `updateOption` broadcasts (the C++
    /// sends them straight to `Td::send_update` from the manager).
    pub(crate) fn flush_option_updates(&mut self) {
        if let Some(option_manager) = self.option_manager.as_mut() {
            for update in option_manager.take_updates() {
                self.send_update(update);
            }
        }
    }

    /// The `DcOptions(server_dc_options)` ctor filter plus the
    /// `ConnectionCreator::on_dc_options` call both config fan-outs
    /// deliver (ConfigManager.cpp:632, 1191): invalid options drop at the
    /// ctor, the sort seed is the `my_id` option
    /// (`G()->get_option_integer("my_id")`).
    fn send_connection_creator_dc_options(&mut self, dc_options: &[telegram_api::DcOption]) {
        let options = dc_options
            .iter()
            .filter_map(StoredDcOption::from_telegram)
            .collect::<Vec<_>>();
        let seed = self
            .option_manager
            .as_ref()
            .map(|option_manager| option_manager.get_option_integer("my_id", 0))
            .unwrap_or(0);
        let is_test_dc = self.global.is_test_dc();
        let Some(connection_creator) = self.connection_creator.as_mut() else {
            tracing::trace!("ConnectionCreator::on_dc_options dropped — no creator");
            return;
        };
        connection_creator.on_dc_options(options, seed, is_test_dc, self.global.binlog_pmc());
    }

    /// Complete the requests a ConfigManager call produced and hold its
    /// follow-up queries. The C++ manager actor sends each result itself;
    /// the inline manager returns them here. The updateOption broadcasts
    /// queued by the manager's option writes go out before the answers,
    /// like the C++ `send_update` calls during the handlers.
    pub(crate) fn apply_config_actions(&mut self, actions: ConfigActions) {
        if actions.refresh_premium_options
            && let Some(option_manager) = self.option_manager.as_mut()
        {
            option_manager.update_premium_options();
        }
        self.flush_option_updates();

        // The process_config fan-outs (ConfigManager.cpp:1191/1196).
        if let Some(dc_options) = &actions.connection_creator_dc_options {
            self.send_connection_creator_dc_options(dc_options);
        }
        if let Some(autologin_token) = &actions.autologin_token
            && let Some(link_manager) = self.link_manager.as_mut()
        {
            link_manager.update_autologin_token(autologin_token.clone());
        }

        for id in actions.ok_ids {
            self.send_result(id, ok_object());
        }
        for (id, error) in actions.errors {
            self.send_error_impl(id, error.code(), error.message());
        }
        for (id, name) in actions.get_option_answers {
            // The C++ wrap_promise answers the raw store value (the async
            // get_option would route back into the ConfigManager).
            let value = self
                .option_manager
                .as_ref()
                .map(|option_manager| {
                    OptionManager::get_option_value_object(&option_manager.get_option(&name))
                })
                .unwrap_or_else(|| td_api::OptionValue::optionValueEmpty(td_api::optionValueEmpty));
            self.send_result(id, requests::option_value_to_object(value));
        }
        for (id, value) in actions.app_config_answers {
            self.send_result(id, requests::json_value_to_object(value));
        }
        for query in actions.queries {
            self.dispatch_config_query(query);
        }
        self.sync_config_timeout();
    }

    /// Mirror the ConfigManager's expire time into the Td's own actor
    /// timeout slot (the C++ manager actor carries its own timer). The
    /// timer methods panic outside a scheduler, so the direct-call tests
    /// skip the mirroring.
    fn sync_config_timeout(&mut self) {
        if !tdactor::Scheduler::in_scheduler_context() {
            return;
        }
        let mut expire_time = self
            .config_manager
            .as_ref()
            .map(ConfigManager::expire_time)
            .unwrap_or(0.0);
        // The recoverer's wakeup shares the Td's single timeout slot: the
        // earliest deadline wins (the C++ actors each carry their own
        // timer), and the fired timeout re-loops both.
        if self.recoverer_wakeup_at != 0.0
            && (expire_time == 0.0 || self.recoverer_wakeup_at < expire_time)
        {
            expire_time = self.recoverer_wakeup_at;
        }
        if expire_time == 0.0 {
            self.cancel_timeout();
        } else {
            self.set_timeout_at(expire_time);
        }
    }

    /// The `G()` reads the recoverer steps need, gathered per call (the C++
    /// recoverer reads them through the global directly). The loop's
    /// clocks and `Session::is_high_loaded()` arrive here too: both come
    /// from the real clock now (nearly equal, like the C++
    /// `Time::now()`/`now_cached()` mix), and the handshake-actor pool
    /// that drives the high-load flag lands with the net step.
    pub(crate) fn recoverer_env(&self) -> RecovererEnv {
        let option_manager = self.option_manager.as_ref();
        #[cfg(test)]
        let (now, now_cached) = self
            .recoverer_clock
            .unwrap_or_else(|| (tdutils::time::now(), tdutils::time::now_cached()));
        #[cfg(not(test))]
        let (now, now_cached) = (tdutils::time::now(), tdutils::time::now_cached());
        RecovererEnv {
            unix_time: self.global.unix_time(),
            my_phone_number: option_manager
                .map(|options| options.get_option_string("my_phone_number", ""))
                .unwrap_or_default(),
            expect_blocking: option_manager
                .map(|options| options.get_option_boolean("expect_blocking", true))
                .unwrap_or(true),
            now,
            now_cached,
            is_high_loaded: false,
            is_test_dc: self.global.is_test_dc(),
            prefer_ipv6: option_manager
                .map(|options| options.get_option_boolean("prefer_ipv6", false))
                .unwrap_or(false),
            dc_txt_domain_name: option_manager
                .map(|options| options.get_option_string("dc_txt_domain_name", ""))
                .unwrap_or_default(),
        }
    }

    /// The recoverer's C++ side effects, executed by the Td: the DNS time
    /// difference write, the dispatched fetches (parked until their
    /// executors land), the ConnectionCreator fan-out (ConfigManager.cpp:632,
    /// applied through the same M.3.9.1 consumer as `process_config`) and
    /// the wakeup timer mirror.
    pub(crate) fn apply_recoverer_actions(&mut self, actions: RecovererActions) {
        if let Some(diff) = actions.dns_time_difference {
            self.global.update_dns_time_difference(diff);
        }
        if let Some(fetch) = actions.fetch_simple_config {
            self.pending_recoverer_fetches
                .push(RecovererFetch::Simple(fetch));
        }
        if let Some(fetch) = actions.fetch_full_config {
            self.pending_recoverer_fetches
                .push(RecovererFetch::Full(fetch));
        }
        if let Some(dc_options) = &actions.connection_creator_dc_options {
            self.send_connection_creator_dc_options(dc_options);
        }
        if let Some(at) = actions.wakeup_at {
            self.recoverer_wakeup_at = at;
        }
        self.sync_config_timeout();
    }

    /// A simple-config fetch result — the C++ fetch actor's promise
    /// (`ConfigRecoverer::on_simple_config`, ConfigManager.cpp:566-588)
    /// followed by its `hangup_shared` share release. Tests drive the
    /// recoverer loop through it; the net step's async dispatch delivers
    /// here.
    pub(crate) fn on_recoverer_simple_config_result(
        &mut self,
        result: Result<SimpleConfigResult, Status>,
    ) {
        let environment = self.recoverer_env();
        let Some(recoverer) = self.config_recoverer.as_mut() else {
            return;
        };
        if !recoverer.simple_config_query_active() {
            // No dispatched query — accepting the delivery would
            // double-release the query's ref-count share below.
            return;
        }
        let actions = recoverer.on_simple_config(result, &environment);
        if recoverer.hangup_shared() {
            self.config_recoverer = None;
        }
        self.apply_recoverer_actions(actions);
    }

    /// A full-config fetch result — `ConfigRecoverer::on_full_config`
    /// (ConfigManager.cpp:626-639) with the same share release.
    pub(crate) fn on_recoverer_full_config_result(
        &mut self,
        result: Result<telegram_api::Config, Status>,
    ) {
        let environment = self.recoverer_env();
        let Some(recoverer) = self.config_recoverer.as_mut() else {
            return;
        };
        if !recoverer.full_config_query_active() {
            return;
        }
        let actions = recoverer.on_full_config(result, &environment);
        if recoverer.hangup_shared() {
            self.config_recoverer = None;
        }
        self.apply_recoverer_actions(actions);
    }

    /// The recoverer's `StateCallback::on_state` leg
    /// (ConfigManager.cpp:804-807): only a plain `Connecting` counts as a
    /// connecting problem. The StateManager itself lands with the net
    /// step — it drives these three forwarders (the C++ registers the
    /// callback at the recoverer's `start_up`, ConfigManager.cpp:820).
    #[allow(dead_code)] // the StateManager registration is a net-step wiring
    pub(crate) fn on_recoverer_connection_state(&mut self, state: RecovererConnectionState) {
        let environment = self.recoverer_env();
        let Some(recoverer) = self.config_recoverer.as_mut() else {
            return;
        };
        let actions = recoverer.on_connection_state(state, &environment);
        self.apply_recoverer_actions(actions);
    }

    /// The recoverer's `StateCallback::on_network` leg
    /// (ConfigManager.cpp:808-811) — every `NetType` but `None` means the
    /// network is up.
    #[allow(dead_code)] // the StateManager registration is a net-step wiring
    pub(crate) fn on_recoverer_net_type(&mut self, net_type: RecovererNetType, generation: u32) {
        let environment = self.recoverer_env();
        let Some(recoverer) = self.config_recoverer.as_mut() else {
            return;
        };
        let actions = recoverer.on_net_type(net_type, generation, &environment);
        self.apply_recoverer_actions(actions);
    }

    /// The recoverer's `StateCallback::on_online` leg
    /// (ConfigManager.cpp:812-815).
    #[allow(dead_code)] // the StateManager registration is a net-step wiring
    pub(crate) fn on_recoverer_online(&mut self, online_flag: bool) {
        let environment = self.recoverer_env();
        let Some(recoverer) = self.config_recoverer.as_mut() else {
            return;
        };
        let actions = recoverer.on_online(online_flag, &environment);
        self.apply_recoverer_actions(actions);
    }

    /// `ConfigManager::on_dc_options_update` (ConfigManager.cpp:1012-1020)
    /// — the Updates `updateDcOptions` fan-in (UpdatesManager.cpp:4539):
    /// persist the update, expire the config, and feed the recoverer (the
    /// C++ forwards the same options unconditionally, so an empty update
    /// clears the recoverer's set too).
    #[allow(dead_code)] // the UpdatesManager fan-in arrives with its step
    pub(crate) fn on_dc_options_update(&mut self, dc_options: &[telegram_api::DcOption]) {
        let recoverer_options: Vec<RecovererDcOption> = dc_options
            .iter()
            .filter_map(StoredDcOption::from_telegram)
            .map(|stored| RecovererDcOption::from_stored(&stored))
            .collect();
        if let Some(config_manager) = self.config_manager.as_mut() {
            config_manager.on_dc_options_update(self.global.binlog_pmc(), dc_options);
        }
        let environment = self.recoverer_env();
        let actions = self
            .config_recoverer
            .as_mut()
            .map(|recoverer| recoverer.on_dc_options_update(recoverer_options, &environment))
            .unwrap_or_default();
        self.apply_recoverer_actions(actions);
    }

    /// The `OptionManager::get_option` ConfigManager branch for the two
    /// content-settings names (cpp:770-787): refresh the content settings,
    /// then answer from the store. Unauthorized clients and bots answer
    /// immediately.
    pub(crate) fn get_option_content_settings(&mut self, id: u64, name: String) {
        let Some(config_manager) = self.config_manager.as_mut() else {
            // Unreachable through the request gate: the manager exists
            // before the getters are answered.
            return self.send_error_impl(id, 500, "TDLib function is not implemented yet");
        };
        let actions = config_manager.get_content_settings(
            Some((id, name)),
            self.auth_manager.is_authorized(),
            self.auth_manager.is_bot(),
            self.close_flag != 0,
        );
        self.apply_config_actions(actions);
    }

    /// The `OptionManager::set_option` ConfigManager branch for
    /// `ignore_sensitive_content_restrictions` (cpp:969-983): the setOption
    /// answers when the `account.setContentSettings` query completes.
    pub(crate) fn set_content_settings_option(&mut self, id: u64, value: bool) {
        let Some(config_manager) = self.config_manager.as_mut() else {
            return self.send_error_impl(id, 500, "TDLib function is not implemented yet");
        };
        let actions = config_manager.set_content_settings(value, Some(id), self.close_flag != 0);
        self.apply_config_actions(actions);
    }

    /// `Requests::on_request(getApplicationConfig)` →
    /// `ConfigManager::get_app_config`.
    pub(crate) fn get_application_config(&mut self, id: u64) {
        let Some(config_manager) = self.config_manager.as_mut() else {
            return self.send_error_impl(id, 500, "TDLib function is not implemented yet");
        };
        let actions =
            config_manager.get_app_config(id, self.auth_manager.is_bot(), self.close_flag != 0);
        self.apply_config_actions(actions);
    }

    /// `NetQueryDispatcher::get_main_dc_id()` — read through the
    /// dispatcher like the C++ (`pingProxy` does the same); 1 before
    /// `init` creates the dispatcher.
    fn get_main_dc_id(&self) -> i32 {
        self.net_query_dispatcher
            .as_ref()
            .map_or(1, |dispatcher| dispatcher.get_main_dc_id().get_value())
    }

    /// `NetQueryDispatcher::dispatch_with_callback` for one ConfigManager
    /// query — the C++ dispatch sites (ConfigManager.cpp:1027, :957, :986,
    /// :1002) with the `actor_shared(this, token)` payload carried by the
    /// callback.
    pub(crate) fn dispatch_config_query(&mut self, query: ConfigQuery) {
        let Some(dispatcher) = self.net_query_dispatcher.as_ref() else {
            // The queries only originate from the ConfigManager after
            // `init` creates the dispatcher; nothing to dispatch into
            // pre-init.
            return;
        };
        let is_bot = self.auth_manager.is_bot();
        let (net_query, token) = build_config_net_query(&query, dispatcher.next_query_id(), is_bot);
        let callback = ConfigQueryCallback::new(token, self.completed_config_queries.clone());
        dispatcher.dispatch_with_callback(net_query, Box::new(callback));
    }

    /// `G()->unix_time()` — the server-time-corrected wall clock; the value
    /// comes from the Global hub inited at `Td::init` (before this, without
    /// a session the server difference is just the local default).
    pub(crate) fn get_unix_time(&self) -> i32 {
        self.global.unix_time()
    }

    /// A completed ConfigManager query — the C++ `ConfigManager::on_result`
    /// token dispatch. The net step calls this when a pending query
    /// completes; tests drive the config flow through it.
    pub(crate) fn on_config_query_result(&mut self, query: ConfigQuery, result: ConfigQueryResult) {
        let close_flag = self.close_flag != 0;
        let is_authorized = self.auth_manager.is_authorized();
        let is_bot = self.auth_manager.is_bot();
        let is_test_dc = self.global.is_test_dc();
        let main_dc_id = self.get_main_dc_id();
        let Some(config_manager) = self.config_manager.as_mut() else {
            return;
        };
        let actions = {
            let pmc = self.global.binlog_pmc();
            let option_manager = self
                .option_manager
                .as_mut()
                .expect("option manager set in Run");
            match (query, result) {
                (
                    ConfigQuery::GetConfig {
                        dc_id: _,
                        reopen_sessions,
                    },
                    ConfigQueryResult::GetConfig(r_config),
                ) => config_manager.on_get_config_result(
                    pmc,
                    r_config,
                    reopen_sessions,
                    close_flag,
                    option_manager,
                    is_test_dc,
                    is_authorized,
                    is_bot,
                    main_dc_id,
                ),
                (
                    ConfigQuery::GetAppConfig { .. },
                    ConfigQueryResult::GetAppConfig(r_app_config),
                ) => config_manager.on_get_app_config_result(
                    pmc,
                    r_app_config,
                    close_flag,
                    option_manager,
                    is_authorized,
                    is_bot,
                ),
                (
                    ConfigQuery::GetContentSettings,
                    ConfigQueryResult::GetContentSettings(r_content_settings),
                ) => config_manager.on_get_content_settings_result(
                    r_content_settings,
                    option_manager,
                    is_bot,
                    close_flag,
                ),
                (
                    ConfigQuery::SetContentSettings { sensitive_enabled },
                    ConfigQueryResult::SetContentSettings(r_set),
                ) => config_manager.on_set_content_settings_result(
                    r_set,
                    sensitive_enabled,
                    option_manager,
                    is_bot,
                    close_flag,
                ),
                _ => {
                    tracing::error!("Mismatched config query result");
                    return;
                }
            }
        };
        self.apply_config_actions(actions);
    }

    /// M.3.8.2 — the net half of `ConfigManager::on_result`
    /// (ConfigManager.cpp:1040-1140): a completed `NetQuery` and its
    /// `actor_shared` link token reconstruct the dispatched query (the
    /// token↔request-constructor match is the C++ provenance the link
    /// token provides), decode via the `fetch_result` port, and feed the
    /// token dispatch above. Mismatched pairs warn and drop.
    pub(crate) fn on_config_net_query_result(&mut self, token: u64, net_query: NetQuery) {
        debug_assert!(net_query.is_ready(), "completion of an unanswered query");
        let Some(query) = config_result::reconstruct_config_query(token, &net_query) else {
            tracing::warn!(
                "Config query completion with token {token} does not match its request bytes"
            );
            return;
        };
        let mut net_query = net_query;
        let result = config_result::decode_config_query_result(&query, &mut net_query);
        self.on_config_query_result(query, result);
    }

    /// M.3.8.2 — drain the dispatcher's parked completions
    /// (token, NetQuery) in dispatch order; the close path runs this for
    /// the dispatcher-stop aborts, the actor flow once the callback
    /// delivery is wired there.
    pub(crate) fn drain_config_net_query_completions(&mut self) {
        let completions = std::mem::take(&mut *self.completed_config_queries.lock().unwrap());
        for (token, net_query) in completions {
            self.on_config_net_query_result(token, net_query);
        }
    }

    /// `Td::send_result` (Td.cpp:1399-1414).
    pub(crate) fn send_result(&mut self, id: u64, object: Object) {
        if id == 0 {
            tracing::error!("Sending a result through send_result with id 0");
            return;
        }

        if self.request_set.remove(&id) {
            let Some(callback) = self.callback.as_mut() else {
                return;
            };
            callback.on_result(id, object);
        }
    }

    /// `Td::send_error_impl` (Td.cpp:1416-1428).
    pub(crate) fn send_error_impl(&mut self, id: u64, code: i32, message: &str) {
        if self.request_set.remove(&id) {
            if code == 0 && message == "Lost promise" {
                panic!(
                    "Lost promise for query {id} in close state {}",
                    self.close_flag
                );
            }
            let Some(callback) = self.callback.as_mut() else {
                return;
            };
            callback.on_error(
                id,
                td_api::Error {
                    code,
                    message: message.to_string(),
                },
            );
        }
    }

    /// `Td::get_parameters` (Td.cpp:1435-1497): validate + split into the
    /// `Td::Parameters` and the `TdDb::Parameters`.
    fn get_parameters(
        &mut self,
        p: td_api::setTdlibParameters,
    ) -> Result<(Parameters, DbParameters), td_api::Error> {
        let error = |code: i32, message: &str| td_api::Error {
            code,
            message: message.to_string(),
        };

        let td_api::setTdlibParameters {
            use_test_dc,
            database_directory,
            files_directory,
            database_encryption_key,
            use_file_database,
            use_chat_info_database,
            use_message_database,
            use_secret_chats,
            api_id,
            api_hash,
            system_language_code,
            device_model,
            system_version,
            application_version,
        } = p;

        // The C++ clean_input_string checks byte-level UTF-8 (impossible to
        // violate in Rust Strings) and sanitizes control characters — the
        // sanitization is observable on the stored values.
        let clean = |s: &str| -> Result<String, td_api::Error> {
            crate::misc::clean_input_string(s)
                .ok_or_else(|| error(400, "Strings must be encoded in UTF-8"))
        };
        let api_hash = clean(&api_hash)?;
        let system_language_code = clean(&system_language_code)?;
        let device_model = clean(&device_model)?;
        let system_version = clean(&system_version)?;
        let application_version = clean(&application_version)?;

        if api_id <= 0 {
            return Err(error(
                400,
                "Valid api_id must be provided. Can be obtained at https://my.telegram.org",
            ));
        }
        if api_hash.is_empty() {
            return Err(error(
                400,
                "Valid api_hash must be provided. Can be obtained at https://my.telegram.org",
            ));
        }

        let parameters = Parameters {
            api_id,
            api_hash: api_hash.clone(),
            use_secret_chats,
        };
        let db_parameters = DbParameters {
            encryption_key: if database_encryption_key.is_empty() {
                tddb::db_key::DbKey::empty()
            } else {
                tddb::db_key::DbKey::password(database_encryption_key)
            },
            database_directory,
            files_directory,
            is_test_dc: use_test_dc,
            use_file_database,
            use_chat_info_database,
            use_message_database,
        };

        // Create MtprotoHeader::Options (Td.cpp:1465-1494).
        let header = &mut self.header_options;
        header.api_id = api_id;
        header.system_language_code = trim_ascii(&system_language_code).to_owned();
        header.device_model = trim_ascii(&device_model).to_owned();
        header.system_version = trim_ascii(&system_version).to_owned();
        header.application_version = trim_ascii(&application_version).to_owned();
        if header.system_language_code.is_empty() {
            return Err(error(400, "System language code must be non-empty"));
        }
        if header.device_model.is_empty() {
            return Err(error(400, "Device model must be non-empty"));
        }
        if header.system_version.is_empty() {
            header.system_version = get_operating_system_version();
        }
        if header.application_version.is_empty() {
            return Err(error(400, "Application version must be non-empty"));
        }
        if header.api_id != 21724 {
            header.application_version += ", TDLib ";
            header.application_version += crate::option_manager::API_VERSION;
        }
        header.language_pack = String::new();
        header.language_code = String::new();
        header.parameters = String::new();
        header.is_emulator = false;

        Ok((parameters, db_parameters))
    }

    /// `Td::start_up` (Td.cpp:438-461): the endianness guard is a compile
    /// error here; the common state + the fake auth state follow.
    fn emit_start_up(&mut self) {
        // The Requests instance and the Global are plain Rust structures —
        // no actor creation needed (the C++ actor bookkeeping in start_up
        // guards its async teardown).
        for update in self.get_fake_current_state() {
            self.send_update(update);
        }
    }

    /// `Td` close: dropping the callback sends the close notification.
    fn close(&mut self) {
        drop(self.callback.take());
    }
}

actor_messages! {
    pub enum TdMessage for Td {
        Request { request_id: u64, function: Function, } => request(request_id, function),
        Close => close_impl(false),
        Destroy => close_impl(true),
        /// A completed ConfigManager query — the C++
        /// `ConfigManager::on_result` dispatch. The net step calls this
        /// when a pending query completes; tests drive the config flow
        /// through it.
        ConfigQueryResult { query: ConfigQuery, result: ConfigQueryResult, } => on_config_query_result(query, result),
        /// A completed recoverer simple-config fetch — the C++ fetch
        /// actor's promise delivery (`ConfigRecoverer::on_simple_config`).
        /// Tests drive the recoverer loop through it; the parked fetch's
        /// live executor (the net step) delivers here.
        RecovererSimpleConfigResult { result: Result<SimpleConfigResult, Status>, } => on_recoverer_simple_config_result(result),
        /// A completed recoverer full-config fetch —
        /// `ConfigRecoverer::on_full_config`; the M.3.7.4 executor is the
        /// production sender.
        RecovererFullConfigResult { result: Result<telegram_api::Config, Status>, } => on_recoverer_full_config_result(result),
    }
}

impl Actor for Td {
    fn start_up(&mut self) {
        self.emit_start_up();
    }

    /// `Td::hangup` (Td.cpp:481-485): the ClientManager's ActorOwn erase
    /// lands here and starts the close machine.
    fn hangup(&mut self) {
        self.close_impl(false);
        self.dec_stop_cnt();
    }

    /// The ConfigManager's expire timer rides the Td's actor timeout slot
    /// (the C++ manager is its own actor with its own timer): the fired
    /// timeout re-requests the expired config, then re-runs the
    /// recoverer's loop (the tdactor `timeout_expired` default calls
    /// `loop`).
    fn timeout_expired(&mut self) {
        let close_flag = self.close_flag != 0;
        let actions = self
            .config_manager
            .as_mut()
            .map(|config_manager| config_manager.on_timeout(close_flag))
            .unwrap_or_default();
        self.apply_config_actions(actions);

        let environment = self.recoverer_env();
        let actions = self
            .config_recoverer
            .as_mut()
            .map(|recoverer| recoverer.loop_(&environment))
            .unwrap_or_default();
        self.apply_recoverer_actions(actions);
    }

    fn tear_down(&mut self) {
        self.close();
    }

    // No `loop_`: the pending-query poll arrives with the net step (the
    // C++ `Td::loop` yields to drive the NetQueryDispatcher).
}

/// The `complete_pending_preauthentication_requests` id sets, collapsed
/// into named kinds (the C++ switches over constructor ids).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum FunctionKind {
    Any,
    GetOption,
    SetOption,
    NetworkStatistics,
    Proxy,
}

impl FunctionKind {
    fn of(function: &Function) -> FunctionKind {
        match function {
            Function::getOption(_) => FunctionKind::GetOption,
            Function::setOption(_) => FunctionKind::SetOption,
            Function::getNetworkStatistics(_)
            | Function::addNetworkStatistics(_)
            | Function::resetNetworkStatistics(_) => FunctionKind::NetworkStatistics,
            Function::addProxy(_)
            | Function::editProxy(_)
            | Function::enableProxy(_)
            | Function::disableProxy(_)
            | Function::removeProxy(_)
            | Function::getProxies(_) => FunctionKind::Proxy,
            _ => FunctionKind::Any,
        }
    }
}

fn ok_object() -> Object {
    Object::ok(td_api::Ok)
}

/// `td::trim` — C `isspace` semantics (space, \t, \n, \v, \f, \r); Rust's
/// `str::trim` is Unicode-aware and would diverge on e.g. NBSP.
fn trim_ascii(s: &str) -> &str {
    let is_space = |c: u8| matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r');
    let bytes = s.as_bytes();
    let mut start = 0;
    while start < bytes.len() && is_space(bytes[start]) {
        start += 1;
    }
    let mut end = bytes.len();
    while end > start && is_space(bytes[end - 1]) {
        end -= 1;
    }
    std::str::from_utf8(&bytes[start..end]).unwrap_or("")
}

/// `get_operating_system_version` (tdutils/td/utils/port/uname.cpp): the
/// default `system_version` when the client passes an empty one. The Linux
/// uname syscall is replaced by the procfs files (same source strings).
fn get_operating_system_version() -> String {
    let read = |path: &str| {
        std::fs::read_to_string(path)
            .map(|s| s.trim().to_owned())
            .ok()
    };
    match (
        read("/proc/sys/kernel/ostype"),
        read("/proc/sys/kernel/osrelease"),
    ) {
        (Some(ostype), Some(osrelease)) => format!("{ostype} {osrelease}"),
        _ => String::new(),
    }
}

/// `Td::is_authentication_request` (Td.cpp:154-186).
pub(crate) fn is_authentication_request(function: &Function) -> bool {
    matches!(
        function,
        Function::setTdlibParameters(_)
            | Function::getAuthorizationState(_)
            | Function::setAuthenticationPhoneNumber(_)
            | Function::checkAuthenticationPremiumPurchase(_)
            | Function::setAuthenticationPremiumPurchaseTransaction(_)
            | Function::sendAuthenticationFirebaseSms(_)
            | Function::reportAuthenticationCodeMissing(_)
            | Function::setAuthenticationEmailAddress(_)
            | Function::resendAuthenticationCode(_)
            | Function::checkAuthenticationEmailCode(_)
            | Function::checkAuthenticationCode(_)
            | Function::registerUser(_)
            | Function::requestQrCodeAuthentication(_)
            | Function::getAuthenticationPasskeyParameters(_)
            | Function::checkAuthenticationPasskey(_)
            | Function::checkAuthenticationWebToken(_)
            | Function::resetAuthenticationEmailAddress(_)
            | Function::checkAuthenticationPassword(_)
            | Function::requestAuthenticationPasswordRecovery(_)
            | Function::checkAuthenticationPasswordRecoveryCode(_)
            | Function::recoverAuthenticationPassword(_)
            | Function::deleteAccount(_)
            | Function::logOut(_)
            | Function::close(_)
            | Function::destroy(_)
            | Function::checkAuthenticationBotToken(_)
    )
}

/// `Td::is_preinitialization_request` (Td.cpp:188-206).
pub(crate) fn is_preinitialization_request(function: &Function) -> bool {
    matches!(
        function,
        Function::getCurrentState(_)
            | Function::setAlarm(_)
            | Function::testUseUpdate(_)
            | Function::testCallEmpty(_)
            | Function::testSquareInt(_)
            | Function::testCallString(_)
            | Function::testCallBytes(_)
            | Function::testCallVectorInt(_)
            | Function::testCallVectorIntObject(_)
            | Function::testCallVectorString(_)
            | Function::testCallVectorStringObject(_)
            | Function::testProxy(_)
    )
}

/// `Td::is_preauthentication_request` (Td.cpp:208-250).
pub(crate) fn is_preauthentication_request(function: &Function) -> bool {
    matches!(
        function,
        Function::getInternalLink(_)
            | Function::getInternalLinkType(_)
            | Function::getLocalizationTargetInfo(_)
            | Function::getLanguagePackInfo(_)
            | Function::getLanguagePackStrings(_)
            | Function::synchronizeLanguagePack(_)
            | Function::addCustomServerLanguagePack(_)
            | Function::setCustomLanguagePack(_)
            | Function::editCustomLanguagePackInfo(_)
            | Function::setCustomLanguagePackString(_)
            | Function::deleteLanguagePack(_)
            | Function::processPushNotification(_)
            | Function::getOption(_)
            | Function::setOption(_)
            | Function::getStorageStatistics(_)
            | Function::getStorageStatisticsFast(_)
            | Function::getDatabaseStatistics(_)
            | Function::setNetworkType(_)
            | Function::getNetworkStatistics(_)
            | Function::addNetworkStatistics(_)
            | Function::resetNetworkStatistics(_)
            | Function::setApplicationVerificationToken(_)
            | Function::getCountries(_)
            | Function::getCountryCode(_)
            | Function::getPhoneNumberInfo(_)
            | Function::getDeepLinkInfo(_)
            | Function::getApplicationConfig(_)
            | Function::saveApplicationLogEvent(_)
            | Function::addProxy(_)
            | Function::editProxy(_)
            | Function::enableProxy(_)
            | Function::disableProxy(_)
            | Function::removeProxy(_)
            | Function::getProxies(_)
            | Function::pingProxy(_)
            | Function::testNetwork(_)
    )
}

/// Convenience: send a request through the typed message channel.
pub fn send_request(actor: &ActorOwn<Td>, request_id: u64, function: Function) {
    tdactor::send_closure_later(
        actor,
        TdMessage::Request {
            request_id,
            function,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_manager::StoredDcOptions;
    use std::sync::{Arc, Mutex};
    use tdactor::{ConcurrentScheduler, create_actor};
    use tdmtproto::telegram_api;

    #[derive(Default)]
    struct CollectCallback {
        responses: Arc<Mutex<Vec<(u64, Object)>>>,
        errors: Arc<Mutex<Vec<(u64, td_api::Error)>>>,
    }

    impl TdCallback for CollectCallback {
        fn on_result(&mut self, id: u64, result: Object) {
            self.responses.lock().unwrap().push((id, result));
        }
        fn on_error(&mut self, id: u64, error: td_api::Error) {
            self.errors.lock().unwrap().push((id, error));
        }
    }

    /// Start a scheduler with one Td actor, pump `start_up`, and hand the
    /// pieces to the test.
    fn run_actor_with(
        callback: &CollectCallback,
        test: impl FnOnce(&ActorOwn<Td>, &CollectCallback, &mut ConcurrentScheduler),
    ) {
        let mut scheduler = ConcurrentScheduler::new(0);
        let actor = {
            let _guard = scheduler.get_main_guard();
            create_actor(
                "Td",
                Td::new(Box::new(CollectCallback {
                    responses: callback.responses.clone(),
                    errors: callback.errors.clone(),
                })),
            )
        };
        scheduler.start();
        let _ = scheduler.run_main(0.01); // process start_up
        test(&actor, callback, &mut scheduler);
        scheduler.finish();
    }

    fn send(
        actor: &ActorOwn<Td>,
        scheduler: &mut ConcurrentScheduler,
        id: u64,
        function: Function,
    ) {
        let send_ptr = scheduler.send_scheduler();
        {
            let _guard = send_ptr.guard(false);
            send_request(actor, id, function);
        }
        let _ = scheduler.run_main(0.01);
    }

    fn responses_of(callback: &CollectCallback, id: u64) -> Vec<Object> {
        callback
            .responses
            .lock()
            .unwrap()
            .iter()
            .filter(|(response_id, _)| *response_id == id)
            .map(|(_, object)| object.clone())
            .collect()
    }

    fn error_of(callback: &CollectCallback, id: u64) -> td_api::Error {
        callback
            .errors
            .lock()
            .unwrap()
            .iter()
            .find(|(error_id, _)| *error_id == id)
            .map(|(_, error)| error.clone())
            .expect("the error response")
    }

    fn auth_state_of(update: &Object) -> String {
        let Object::updateAuthorizationState(u) = update else {
            return "not-an-auth-state".to_string();
        };
        match &u.authorization_state {
            td_api::AuthorizationState::authorizationStateWaitTdlibParameters(_) => {
                "waitTdlibParameters"
            }
            td_api::AuthorizationState::authorizationStateWaitPhoneNumber(_) => "waitPhoneNumber",
            td_api::AuthorizationState::authorizationStateReady(_) => "ready",
            td_api::AuthorizationState::authorizationStateClosing(_) => "closing",
            td_api::AuthorizationState::authorizationStateClosed(_) => "closed",
            _ => "other",
        }
        .to_string()
    }

    fn temp_db_dir(name: &str) -> String {
        let dir = std::env::temp_dir().join(format!("td-rs-m1-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.to_string_lossy().into_owned()
    }

    fn set_tdlib_parameters(database_directory: String, use_file_database: bool) -> Function {
        Function::setTdlibParameters(td_api::setTdlibParameters {
            use_test_dc: false,
            database_directory,
            files_directory: String::new(),
            database_encryption_key: Vec::new(),
            use_file_database,
            use_chat_info_database: false,
            use_message_database: false,
            use_secret_chats: false,
            api_id: 94575,
            api_hash: "a3406de8d171bb422bb6ddf3bbd800e2".to_string(),
            system_language_code: "en".to_string(),
            device_model: "Desktop".to_string(),
            system_version: "Test".to_string(),
            application_version: "1.0".to_string(),
        })
    }

    #[test]
    fn startup_emits_common_state_then_auth_state() {
        let callback = CollectCallback::default();
        run_actor_with(&callback, |_actor, callback, scheduler| {
            let _ = scheduler.run_main(0.01);
            let responses = callback.responses.lock().unwrap();
            // version, commit_hash, then authorizationStateWaitTdlibParameters.
            assert_eq!(responses.len(), 3);
            assert_eq!(responses[0].0, 0);
            assert!(matches!(
                &responses[0].1,
                Object::updateOption(u) if u.name == "version"
            ));
            assert!(matches!(
                &responses[1].1,
                Object::updateOption(u) if u.name == "commit_hash"
            ));
            assert_eq!(auth_state_of(&responses[2].1), "waitTdlibParameters");
        });
    }

    #[test]
    fn synchronous_request_answers_immediately() {
        let callback = CollectCallback::default();
        run_actor_with(&callback, |actor, callback, scheduler| {
            send(
                actor,
                scheduler,
                7,
                Function::getOption(td_api::getOption {
                    name: "version".to_string(),
                }),
            );
            assert!(responses_of(callback, 7).iter().any(
                |object| matches!(object, Object::optionValueString(v) if v.value == "1.8.67")
            ));
        });
    }

    #[test]
    fn wait_parameters_gating_matches_cpp() {
        let callback = CollectCallback::default();
        run_actor_with(&callback, |actor, callback, scheduler| {
            // Unclassified (getMe) and authentication (logOut) requests: the
            // C++ WaitParameters gate 400s both (Td.cpp:353-356).
            send(actor, scheduler, 8, Function::getMe(td_api::getMe));
            send(actor, scheduler, 9, Function::logOut(td_api::logOut));
            for id in [8, 9] {
                let error = error_of(callback, id);
                assert_eq!(error.code, 400);
                assert_eq!(
                    error.message,
                    "Initialization parameters are needed: call setTdlibParameters first"
                );
            }
        });
    }

    #[test]
    fn get_authorization_state_returns_the_bare_state() {
        let callback = CollectCallback::default();
        run_actor_with(&callback, |actor, callback, scheduler| {
            send(
                actor,
                scheduler,
                10,
                Function::getAuthorizationState(td_api::getAuthorizationState),
            );
            assert!(
                responses_of(callback, 10).iter().any(|object| matches!(
                    object,
                    Object::authorizationStateWaitTdlibParameters(_)
                ))
            );
        });
    }

    #[test]
    fn preauth_requests_queue_without_a_response() {
        let callback = CollectCallback::default();
        run_actor_with(&callback, |actor, callback, scheduler| {
            send(
                actor,
                scheduler,
                11,
                Function::setOption(td_api::setOption {
                    name: "message_unload_delay".to_string(),
                    value: td_api::OptionValue::optionValueInteger(td_api::optionValueInteger {
                        value: 59,
                    }),
                }),
            );
            send(
                actor,
                scheduler,
                12,
                Function::getOption(td_api::getOption {
                    name: "database_directory".to_string(),
                }),
            );
            assert!(
                callback
                    .responses
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|(id, _)| *id == 0),
                "no request responses in WaitParameters"
            );
            assert!(callback.errors.lock().unwrap().is_empty());
        });
    }

    /// The M.1 gate: setTdlibParameters validates, opens the database, runs
    /// the init sequence (options over the pmc, AuthManager restore), and
    /// the client lands in Run.
    #[test]
    fn set_tdlib_parameters_reaches_run() {
        let callback = CollectCallback::default();
        let dir = temp_db_dir("run");
        let db_dir = dir.clone();
        run_actor_with(&callback, move |actor, callback, scheduler| {
            send(actor, scheduler, 20, set_tdlib_parameters(db_dir, false));

            // The ok response.
            assert!(
                responses_of(callback, 20)
                    .iter()
                    .any(|object| matches!(object, Object::ok(_)))
            );

            // The update stream ends with WaitPhoneNumber before the ok.
            {
                let responses = callback.responses.lock().unwrap();
                let ok_pos = responses
                    .iter()
                    .position(|(id, object)| *id == 20 && matches!(object, Object::ok(_)))
                    .expect("the ok response");
                let auth_updates: Vec<String> = responses[..ok_pos]
                    .iter()
                    .filter(|(id, _)| *id == 0)
                    .map(|(_, object)| auth_state_of(object))
                    .filter(|state| state != "not-an-auth-state")
                    .collect();
                assert_eq!(
                    auth_updates.last().map(String::as_str),
                    Some("waitPhoneNumber")
                );
            }

            // In Run the unauthorized gate applies: getMe is none of the
            // exemptions (preauth/preinit/auth).
            send(actor, scheduler, 21, Function::getMe(td_api::getMe));
            let error = error_of(callback, 21);
            assert_eq!((error.code, error.message.as_str()), (401, "Unauthorized"));

            // The preinit echo set works in Run (exempt from the gate).
            send(
                actor,
                scheduler,
                22,
                Function::testSquareInt(td_api::testSquareInt { x: 7 }),
            );
            assert!(
                responses_of(callback, 22)
                    .iter()
                    .any(|object| matches!(object, Object::testInt(v) if v.value == 49))
            );

            // getAuthorizationState in Run answers the bare state.
            send(
                actor,
                scheduler,
                23,
                Function::getAuthorizationState(td_api::getAuthorizationState),
            );
            assert!(
                responses_of(callback, 23)
                    .iter()
                    .any(|object| matches!(object, Object::authorizationStateWaitPhoneNumber(_)))
            );

            // testUseUpdate answers the C++ null-result 404.
            send(
                actor,
                scheduler,
                24,
                Function::testUseUpdate(td_api::testUseUpdate),
            );
            assert!(responses_of(callback, 24)
                .iter()
                .any(|object| matches!(object, Object::error(e) if e.code == 404 && e.message == "Not Found")));

            scheduler.finish();
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A setOption queued during WaitParameters executes after init.
    #[test]
    fn pending_preauth_replays_after_init() {
        let callback = CollectCallback::default();
        let dir = temp_db_dir("replay");
        let db_dir = dir.clone();
        run_actor_with(&callback, move |actor, callback, scheduler| {
            send(
                actor,
                scheduler,
                30,
                Function::setOption(td_api::setOption {
                    name: "use_storage_optimizer".to_string(),
                    value: td_api::OptionValue::optionValueBoolean(td_api::optionValueBoolean {
                        value: true,
                    }),
                }),
            );
            assert!(callback.errors.lock().unwrap().is_empty());

            send(actor, scheduler, 31, set_tdlib_parameters(db_dir, false));
            assert!(
                responses_of(callback, 31)
                    .iter()
                    .any(|object| matches!(object, Object::ok(_)))
            );
            // The replayed setOption answers ok after init.
            assert!(
                responses_of(callback, 30)
                    .iter()
                    .any(|object| matches!(object, Object::ok(_)))
            );
            scheduler.finish();
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Options persist through the binlog config pmc and replay on reopen.
    #[test]
    fn options_persist_across_reopen() {
        let dir = temp_db_dir("persist");
        {
            let callback = CollectCallback::default();
            run_actor_with(&callback, |actor, callback, scheduler| {
                send(
                    actor,
                    scheduler,
                    40,
                    set_tdlib_parameters(dir.clone(), false),
                );
                assert!(
                    responses_of(callback, 40)
                        .iter()
                        .any(|object| matches!(object, Object::ok(_)))
                );
                send(
                    actor,
                    scheduler,
                    41,
                    Function::setOption(td_api::setOption {
                        name: "x".to_string(),
                        value: td_api::OptionValue::optionValueString(td_api::optionValueString {
                            value: "hello".to_string(),
                        }),
                    }),
                );
                assert!(
                    responses_of(callback, 41)
                        .iter()
                        .any(|object| matches!(object, Object::ok(_)))
                );
                // A getOption in Run answers from the store.
                send(
                    actor,
                    scheduler,
                    42,
                    Function::getOption(td_api::getOption {
                        name: "x".to_string(),
                    }),
                );
                assert!(responses_of(callback, 42).iter().any(
                    |object| matches!(object, Object::optionValueString(v) if v.value == "hello")
                ));
                scheduler.finish();
            });
        }

        {
            let callback = CollectCallback::default();
            run_actor_with(&callback, |actor, callback, scheduler| {
                send(
                    actor,
                    scheduler,
                    43,
                    set_tdlib_parameters(dir.clone(), false),
                );
                assert!(
                    responses_of(callback, 43)
                        .iter()
                        .any(|object| matches!(object, Object::ok(_)))
                );
                send(
                    actor,
                    scheduler,
                    44,
                    Function::getOption(td_api::getOption {
                        name: "x".to_string(),
                    }),
                );
                assert!(responses_of(callback, 44).iter().any(
                    |object| matches!(object, Object::optionValueString(v) if v.value == "hello")
                ));
                scheduler.finish();
            });
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The close machine in Run: ok, then the Closing/Closed updates, then
    /// requests abort with 500.
    #[test]
    fn close_request_runs_the_close_machine() {
        let callback = CollectCallback::default();
        let dir = temp_db_dir("close");
        let db_dir = dir.clone();
        run_actor_with(&callback, move |actor, callback, scheduler| {
            send(actor, scheduler, 50, set_tdlib_parameters(db_dir, false));
            assert!(
                responses_of(callback, 50)
                    .iter()
                    .any(|object| matches!(object, Object::ok(_)))
            );

            send(actor, scheduler, 51, Function::close(td_api::close));
            assert!(
                responses_of(callback, 51)
                    .iter()
                    .any(|object| matches!(object, Object::ok(_)))
            );
            let auth_updates: Vec<String> = {
                let responses = callback.responses.lock().unwrap();
                let closed_pos = responses
                    .iter()
                    .position(|(_, object)| auth_state_of(object) == "closed")
                    .expect("the closed update");
                responses[..=closed_pos]
                    .iter()
                    .filter(|(id, _)| *id == 0)
                    .map(|(_, object)| auth_state_of(object))
                    .filter(|state| state != "not-an-auth-state")
                    .collect()
            };
            assert_eq!(auth_updates.last().map(String::as_str), Some("closed"));
            assert!(
                auth_updates
                    .iter()
                    .rev()
                    .skip(1)
                    .any(|state| state == "closing"),
                "closing precedes closed"
            );

            // After the close everything aborts.
            send(actor, scheduler, 52, Function::getMe(td_api::getMe));
            let error = error_of(callback, 52);
            assert_eq!(
                (error.code, error.message.as_str()),
                (500, "Request aborted")
            );
            scheduler.finish();
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Closing during WaitParameters: no database to close, the machine
    /// still completes and aborts queued requests.
    #[test]
    fn close_in_wait_parameters_aborts_everything() {
        let callback = CollectCallback::default();
        run_actor_with(&callback, |actor, callback, scheduler| {
            // Queue a preauth request first.
            send(
                actor,
                scheduler,
                60,
                Function::getOption(td_api::getOption {
                    name: "database_directory".to_string(),
                }),
            );
            send(actor, scheduler, 61, Function::close(td_api::close));
            assert!(
                responses_of(callback, 61)
                    .iter()
                    .any(|object| matches!(object, Object::ok(_)))
            );
            let auth_updates: Vec<String> = {
                let responses = callback.responses.lock().unwrap();
                responses
                    .iter()
                    .filter(|(id, _)| *id == 0)
                    .map(|(_, object)| auth_state_of(object))
                    .filter(|state| state != "not-an-auth-state")
                    .collect()
            };
            assert!(auth_updates.contains(&"closing".to_string()));
            assert!(auth_updates.contains(&"closed".to_string()));

            // The queued getOption aborts (500), not answered.
            let error = error_of(callback, 60);
            assert_eq!(
                (error.code, error.message.as_str()),
                (500, "Request aborted")
            );
            scheduler.finish();
        });
    }

    /// The M.3.7.5 wiring gate: a no-config Td creates the recoverer at
    /// init, the StateCallback legs drive its loop, the dispatched fetches
    /// park, and the canned results apply — the recovered dc options land
    /// and a full config is accepted (the plan's integration verify).
    #[test]
    fn recoverer_loop_applies_canned_results() {
        let callback = CollectCallback::default();
        let dir = temp_db_dir("recoverer-loop");
        let db_dir = dir.clone();
        let mut td = Td::new(Box::new(CollectCallback {
            responses: callback.responses.clone(),
            errors: callback.errors.clone(),
        }));
        let Function::setTdlibParameters(parameters) = set_tdlib_parameters(db_dir, false) else {
            unreachable!("setTdlibParameters helper");
        };
        td.on_set_tdlib_parameters(20, parameters);

        // The recoverer exists; the empty saved update left no options.
        assert!(
            td.config_recoverer
                .as_ref()
                .expect("recoverer at init")
                .dc_options()
                .is_empty()
        );

        // The ConnectionCreator's start_up replay on the fresh database
        // (ConnectionCreator.cpp:1099-1106): the empty update persisted
        // and the set holds the prod defaults only.
        let creator = td.connection_creator.as_ref().expect("creator at init");
        assert_eq!(creator.dc_options().len(), 33);
        assert!(
            StoredDcOptions::parse(&td.global.binlog_pmc().get_raw(b"dc_options"))
                .expect("the empty update persisted")
                .options
                .is_empty()
        );

        // Network up: no connecting problem yet, so nothing dispatches.
        td.on_recoverer_net_type(RecovererNetType::WiFi, 0);
        assert!(td.pending_recoverer_fetches.is_empty());

        // A plain Connecting is the connecting problem; the wakeup arms at
        // the max-connecting delay, the fetch waits for the gate.
        td.on_recoverer_connection_state(RecovererConnectionState::Connecting);
        assert!(td.recoverer_wakeup_at > 0.0);
        assert!(td.pending_recoverer_fetches.is_empty());

        // Pin the loop clock past the gates but under the offline success
        // window (the M.3.7.2 anchoring: now()+300, so the delivered
        // configs' expiry stamps stay in the future); the floor keeps the
        // `as i32` dates exact for the dns-difference assert.
        let anchor = (tdutils::time::now() + 300.0).floor();
        td.recoverer_clock = Some((anchor, anchor));
        td.on_recoverer_connection_state(RecovererConnectionState::Connecting);
        assert!(
            td.config_recoverer
                .as_ref()
                .unwrap()
                .simple_config_query_active()
        );
        assert!(matches!(
            td.pending_recoverer_fetches.last(),
            Some(RecovererFetch::Simple(_))
        ));

        // The canned simple config lands: the http date feeds the dns
        // difference, the rule's endpoint becomes the recovered options,
        // the query share releases, and the loop dispatches the
        // full-config fetch over them.
        let expires = td.global.unix_time() + 1000;
        td.on_recoverer_simple_config_result(Ok(SimpleConfigResult {
            r_http_date: Ok(anchor as i32 + 1000),
            r_config: Ok(telegram_api::help_ConfigSimple {
                date: anchor as i32,
                expires,
                rules: vec![telegram_api::AccessPointRule {
                    phone_prefix_rules: String::new(),
                    dc_id: 2,
                    ips: vec![telegram_api::IpPort::ipPort(telegram_api::ipPort {
                        ipv4: 0x0909_0909,
                        port: 443,
                    })],
                }],
            }),
        }));
        let recoverer = td.config_recoverer.as_ref().unwrap();
        assert!(!recoverer.simple_config_query_active());
        assert_eq!(recoverer.dc_options().len(), 1);
        assert_eq!(recoverer.dc_options()[0].ip_address, "9.9.9.9");
        // The even alternation turn reads the http date: anchor+1000
        // against the pinned env clock.
        assert_eq!(td.global.get_dns_time_difference(), 1000.0);
        assert!(matches!(
            td.pending_recoverer_fetches.last(),
            Some(RecovererFetch::Full(_))
        ));
        assert!(recoverer.full_config_query_active());

        // The canned full config lands: accepted, the query share
        // releases, and no further fetch dispatches. The options ride the
        // M.3.9.1 fan-out into the ConnectionCreator
        // (ConfigManager.cpp:632): sorted, persisted, merged onto the
        // defaults.
        let parked = td.pending_recoverer_fetches.len();
        td.on_recoverer_full_config_result(Ok(telegram_api::Config {
            this_dc: 2,
            date: anchor as i32,
            expires,
            dc_options: vec![telegram_api::DcOption {
                flags: 0,
                ipv6: false,
                media_only: false,
                tcpo_only: false,
                cdn: false,
                r#static: false,
                this_port_only: false,
                id: 2,
                ip_address: "149.154.167.51".to_string(),
                port: 443,
                secret: Vec::new(),
            }],
            ..Default::default()
        }));
        let recoverer = td.config_recoverer.as_ref().unwrap();
        assert!(!recoverer.full_config_query_active());
        assert_eq!(td.pending_recoverer_fetches.len(), parked);
        assert!(td.recoverer_wakeup_at > 0.0);
        let creator = td.connection_creator.as_ref().expect("creator at init");
        let stored = StoredDcOptions::parse(&td.global.binlog_pmc().get_raw(b"dc_options"))
            .expect("the sorted update persisted");
        assert_eq!(stored.options.len(), 1);
        assert_eq!(stored.options[0].ip_address(), "149.154.167.51");
        assert_eq!(stored.options[0].raw_dc_id(), 2);
        // The prod defaults table (33 entries, dc2 two addresses) plus the
        // sorted update.
        assert_eq!(creator.dc_options().len(), 33 + 1);
        assert_eq!(
            creator.dc_options().last().expect("the merged update").ip,
            b"149.154.167.51"
        );

        // A stray delivery without a dispatched query is a no-op — the
        // share release must not run.
        let parked = td.pending_recoverer_fetches.len();
        td.on_recoverer_simple_config_result(Err(Status::error(500, "stray")));
        assert_eq!(td.pending_recoverer_fetches.len(), parked);

        // The close drops the recoverer with the manager
        // (ConfigManager.cpp:892-896) and the M.3.9.1 consumers with the
        // net/managers (Td.cpp:690/547).
        td.close_impl(false);
        assert!(td.config_recoverer.is_none());
        assert!(td.connection_creator.is_none());
        assert!(td.link_manager.is_none());
        assert!(td.pending_recoverer_fetches.is_empty());
        assert_eq!(td.recoverer_wakeup_at, 0.0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `ConfigManager::on_dc_options_update` through the Td: the update
    /// persists, the config expires now, and the recoverer's set follows
    /// (an empty update clears it) — the Updates `updateDcOptions` fan-in
    /// parity (ConfigManager.cpp:1012-1020).
    #[test]
    fn dc_options_update_feeds_the_recoverer() {
        let callback = CollectCallback::default();
        let dir = temp_db_dir("recoverer-dc-options");
        let db_dir = dir.clone();
        let mut td = Td::new(Box::new(CollectCallback {
            responses: callback.responses.clone(),
            errors: callback.errors.clone(),
        }));
        let Function::setTdlibParameters(parameters) = set_tdlib_parameters(db_dir.clone(), false)
        else {
            unreachable!("setTdlibParameters helper");
        };
        td.on_set_tdlib_parameters(21, parameters);

        let option = telegram_api::DcOption {
            flags: 0,
            ipv6: false,
            media_only: false,
            tcpo_only: false,
            cdn: false,
            r#static: false,
            this_port_only: false,
            id: 2,
            ip_address: "149.154.175.50".to_string(),
            port: 443,
            secret: Vec::new(),
        };
        td.on_dc_options_update(std::slice::from_ref(&option));
        assert!(
            !td.global
                .binlog_pmc()
                .get_raw(b"dc_options_update")
                .is_empty()
        );
        let recoverer = td.config_recoverer.as_ref().unwrap();
        assert_eq!(recoverer.dc_options().len(), 1);
        assert_eq!(recoverer.dc_options()[0].ip_address, "149.154.175.50");
        assert!(td.config_manager.as_ref().unwrap().expire_time() > 0.0);

        td.close_impl(false);

        // The saved update replays into a fresh Td's recoverer at init
        // (ConfigManager.cpp:866-868).
        let mut second = Td::new(Box::new(CollectCallback::default()));
        let Function::setTdlibParameters(parameters) = set_tdlib_parameters(db_dir.clone(), false)
        else {
            unreachable!("setTdlibParameters helper");
        };
        second.on_set_tdlib_parameters(22, parameters);
        assert_eq!(
            second.config_recoverer.as_ref().unwrap().dc_options().len(),
            1
        );

        // An empty update clears both the pmc entry and the set.
        second.on_dc_options_update(&[]);
        assert!(
            second
                .global
                .binlog_pmc()
                .get_raw(b"dc_options_update")
                .is_empty()
        );
        assert!(
            second
                .config_recoverer
                .as_ref()
                .unwrap()
                .dc_options()
                .is_empty()
        );

        second.close_impl(false);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn send_config_result(
        actor: &ActorOwn<Td>,
        scheduler: &mut ConcurrentScheduler,
        query: ConfigQuery,
        result: ConfigQueryResult,
    ) {
        let send_ptr = scheduler.send_scheduler();
        {
            let _guard = send_ptr.guard(false);
            tdactor::send_closure_later(actor, TdMessage::ConfigQueryResult { query, result });
        }
        let _ = scheduler.run_main(0.01);
    }

    /// The ConfigManager step gate, white-box: `setTdlibParameters` runs the
    /// init sequence and dispatches the post-init config fetch
    /// (`ConfigManager::start_up` → `help.getConfig`).
    #[test]
    fn post_init_config_fetch_is_dispatched() {
        let callback = CollectCallback::default();
        let dir = temp_db_dir("config-fetch");
        let db_dir = dir.clone();
        let mut td = Td::new(Box::new(CollectCallback {
            responses: callback.responses.clone(),
            errors: callback.errors.clone(),
        }));
        let Function::setTdlibParameters(parameters) = set_tdlib_parameters(db_dir, false) else {
            unreachable!("setTdlibParameters helper");
        };
        td.on_set_tdlib_parameters(20, parameters);
        assert_eq!(td.state, State::Run);
        // The start-up GetConfig went through the dispatcher; without a
        // session-inited DC the slice's DC gate completes it with the C++
        // "No such dc" error through the token-8 callback.
        assert!(td.sent_net_queries.lock().unwrap().is_empty());
        {
            let completed = td.completed_config_queries.lock().unwrap();
            assert_eq!(completed.len(), 1);
            assert_eq!(completed[0].0, 8);
            assert_eq!(completed[0].1.error().message(), "No such dc DcId{1}");
        }
        td.close_impl(false);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Feeding the post-init `GetConfig` result applies the server config to
    /// the options and broadcasts the `updateOption` updates.
    #[test]
    fn config_result_applies_options_and_broadcasts() {
        let callback = CollectCallback::default();
        let dir = temp_db_dir("config-apply");
        let db_dir = dir.clone();
        let mut td = Td::new(Box::new(CollectCallback {
            responses: callback.responses.clone(),
            errors: callback.errors.clone(),
        }));
        let Function::setTdlibParameters(parameters) = set_tdlib_parameters(db_dir, false) else {
            unreachable!("setTdlibParameters helper");
        };
        td.on_set_tdlib_parameters(20, parameters);
        // The dispatched query kind the config flow drives — the start-up
        // GetConfig (the only in-flight query after init). Feed its answer
        // through the parked completion (the M.3.8.2 routing path), so the
        // in-flight count and the completion stay paired.
        let query = ConfigQuery::GetConfig {
            dc_id: 1,
            reopen_sessions: false,
        };

        let config = telegram_api::Config {
            // flags.2 carries the lang fields, flags.16 the autologin
            // token (conditional on the wire).
            flags: 4 | (1 << 16),
            this_dc: 1,
            date: 1000,
            expires: 2000,
            test_mode: false,
            webfile_dc_id: 4,
            tmp_sessions: 100,
            edit_time_limit: 172800,
            suggested_lang_code: "en".to_string(),
            lang_pack_version: 5,
            caption_length_max: 5000,
            message_length_max: 50000,
            me_url_prefix: "https://t.me".to_string(),
            dc_options: vec![
                telegram_api::DcOption {
                    id: 2,
                    ip_address: "149.154.167.51".to_string(),
                    port: 443,
                    ..telegram_api::DcOption::default()
                },
                // The DcOptions ctor drops the invalid dc id
                // (DcOptions.h:245-252).
                telegram_api::DcOption {
                    id: 0,
                    ip_address: "1.2.3.4".to_string(),
                    port: 443,
                    ..telegram_api::DcOption::default()
                },
            ],
            autologin_token: "token".to_string(),
            ..Default::default()
        };
        let mut answer = Vec::new();
        config.store_boxed(&mut answer);
        let (mut net_query, token) = build_config_net_query(
            &query,
            td.net_query_dispatcher.as_ref().unwrap().next_query_id(),
            false,
        );
        net_query.set_ok(answer);
        {
            let mut completions = td.completed_config_queries.lock().unwrap();
            completions.clear();
            completions.push((token, net_query));
        }
        td.drain_config_net_query_completions();
        // The ok config triggers the C++ follow-up app-config reload
        // (process_config → reload_app_config) — a fresh GetAppConfig
        // dispatch parks its own DC-gate completion.
        assert_eq!(td.completed_config_queries.lock().unwrap().len(), 1);
        td.drain_config_net_query_completions();
        assert!(td.completed_config_queries.lock().unwrap().is_empty());

        // The M.3.9.1 fan-outs (ConfigManager.cpp:1191/1196): the
        // DcOptions ctor dropped the invalid dc id, the ConnectionCreator
        // sorted and persisted the update onto the defaults, and the
        // token reached the LinkManager.
        let stored = StoredDcOptions::parse(&td.global.binlog_pmc().get_raw(b"dc_options"))
            .expect("the sorted update persisted");
        assert_eq!(stored.options.len(), 1);
        assert_eq!(stored.options[0].ip_address(), "149.154.167.51");
        assert_eq!(
            td.connection_creator
                .as_ref()
                .expect("creator at init")
                .dc_options()
                .len(),
            34
        );
        assert_eq!(
            td.link_manager
                .as_ref()
                .expect("link manager at init")
                .autologin_token(),
            "token"
        );

        let responses = callback.responses.lock().unwrap();
        let option_updates: Vec<(String, td_api::OptionValue)> = responses
            .iter()
            .filter_map(|(id, object)| {
                if *id != 0 {
                    return None;
                }
                let Object::updateOption(update) = object else {
                    return None;
                };
                Some((update.name.clone(), update.value.clone()))
            })
            .collect();
        // Drop the guard before close: the close path pushes its final
        // notification through the same callback lock.
        drop(responses);
        // The main-DC-gated non-internal option lands.
        assert!(option_updates.iter().any(|(name, value)| {
            name == "suggested_language_pack_id"
                && matches!(value, td_api::OptionValue::optionValueString(v) if v.value == "en")
        }));
        // The internal options (session_count, webfile_dc_id,
        // edit_time_limit) are stored but not broadcast, like the C++.
        for internal_name in ["session_count", "webfile_dc_id", "edit_time_limit"] {
            assert!(
                !option_updates.iter().any(|(name, _)| name == internal_name),
                "{internal_name} must not broadcast"
            );
        }
        // The clamped caption limit broadcasts.
        assert!(option_updates.iter().any(|(name, value)| {
            name == "message_caption_length_max"
                && matches!(value, td_api::OptionValue::optionValueInteger(v) if v.value == 5000)
        }));

        td.close_impl(false);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// M.3.8.2: dispatching the parked queries registers them by net query
    /// id, and a completed `help.getConfig` NetQuery routes by that id —
    /// token-checked — through the drain into the option-broadcast path.
    #[test]
    fn net_query_completions_route_by_dispatch_id() {
        let callback = CollectCallback::default();
        let dir = temp_db_dir("config-net-route");
        let db_dir = dir.clone();
        let mut td = Td::new(Box::new(CollectCallback {
            responses: callback.responses.clone(),
            errors: callback.errors.clone(),
        }));
        let Function::setTdlibParameters(parameters) = set_tdlib_parameters(db_dir, false) else {
            unreachable!("setTdlibParameters helper");
        };
        td.on_set_tdlib_parameters(20, parameters);
        assert_eq!(td.config_manager.as_ref().unwrap().config_sent_cnt(), 1);

        // The start-up GetConfig dispatched through the M.3.8.1 dispatcher
        // and parked its DC-gate error completion; replace it with the
        // session-delivered ok answer (the exact NetQuery the dispatch
        // layer built).
        let query = ConfigQuery::GetConfig {
            dc_id: 1,
            reopen_sessions: false,
        };
        let config = telegram_api::Config {
            flags: 4, // flags.2 carries the lang fields on the wire
            this_dc: 1,
            date: 1000,
            expires: 2000,
            test_mode: false,
            webfile_dc_id: 4,
            tmp_sessions: 100,
            edit_time_limit: 172800,
            suggested_lang_code: "en".to_string(),
            lang_pack_version: 5,
            caption_length_max: 5000,
            message_length_max: 50000,
            me_url_prefix: "https://t.me".to_string(),
            ..Default::default()
        };
        let mut answer = Vec::new();
        config.store_boxed(&mut answer);
        let (mut net_query, token) = build_config_net_query(
            &query,
            td.net_query_dispatcher.as_ref().unwrap().next_query_id(),
            false,
        );
        net_query.set_ok(answer);
        {
            let mut completions = td.completed_config_queries.lock().unwrap();
            completions.clear();
            completions.push((token, net_query));
        }
        td.drain_config_net_query_completions();

        // The ok config triggers the C++ follow-up app-config reload
        // (process_config → reload_app_config, config_manager.rs) — a
        // fresh GetAppConfig dispatch parks its own DC-gate completion
        // with the token-1 callback.
        {
            let completions = td.completed_config_queries.lock().unwrap();
            assert_eq!(completions.len(), 1);
            assert_eq!(completions[0].0, 1, "the app-config reload token");
        }
        td.drain_config_net_query_completions();
        assert!(td.completed_config_queries.lock().unwrap().is_empty());
        assert_eq!(td.config_manager.as_ref().unwrap().config_sent_cnt(), 0);
        let responses = callback.responses.lock().unwrap();
        assert!(responses.iter().any(|(id, object)| {
            *id == 0
                && matches!(
                    object,
                    Object::updateOption(update) if update.name == "message_caption_length_max"
                )
        }));
        drop(responses);
        td.close_impl(false);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A completion whose token does not match its request bytes is
    /// dropped with a warning — the manager state stays untouched.
    #[test]
    fn token_request_mismatch_completion_is_dropped() {
        let callback = CollectCallback::default();
        let dir = temp_db_dir("config-net-unknown");
        let db_dir = dir.clone();
        let mut td = Td::new(Box::new(CollectCallback {
            responses: callback.responses.clone(),
            errors: callback.errors.clone(),
        }));
        let Function::setTdlibParameters(parameters) = set_tdlib_parameters(db_dir, false) else {
            unreachable!("setTdlibParameters helper");
        };
        td.on_set_tdlib_parameters(20, parameters);
        assert_eq!(td.config_manager.as_ref().unwrap().config_sent_cnt(), 1);

        // Token 1 (help.getAppConfig) over help.getConfig request bytes —
        // the provenance check fails and the completion drops.
        let query = ConfigQuery::GetConfig {
            dc_id: 1,
            reopen_sessions: false,
        };
        let (mut net_query, _) = build_config_net_query(&query, 999, false);
        net_query.set_error(crate::config_result::request_aborted_error());
        td.on_config_net_query_result(1, net_query);
        assert_eq!(td.config_manager.as_ref().unwrap().config_sent_cnt(), 1);

        td.close_impl(false);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// M.3.8.2 close-drop: the dispatcher-stop abort (the exact completion
    /// `Td::clear` feeds every still-in-flight query) routes through the
    /// token dispatch — the in-flight `config_sent_cnt` returns to zero —
    /// and `close` leaves the parked buffers empty, mirroring the
    /// dispatcher stop at `Td::clear` (Td.cpp:663).
    #[test]
    fn close_routes_request_aborted_through_the_dispatch() {
        let callback = CollectCallback::default();
        let dir = temp_db_dir("config-net-close");
        let db_dir = dir.clone();
        let mut td = Td::new(Box::new(CollectCallback {
            responses: callback.responses.clone(),
            errors: callback.errors.clone(),
        }));
        let Function::setTdlibParameters(parameters) = set_tdlib_parameters(db_dir, false) else {
            unreachable!("setTdlibParameters helper");
        };
        td.on_set_tdlib_parameters(20, parameters);
        assert_eq!(td.config_manager.as_ref().unwrap().config_sent_cnt(), 1);

        // The start-up GetConfig is the one in-flight query; the abort
        // completion the close path produces for it.
        let query = ConfigQuery::GetConfig {
            dc_id: 1,
            reopen_sessions: false,
        };
        let (mut net_query, token) = build_config_net_query(&query, 1, false);
        net_query.set_error(crate::config_result::request_aborted_error());
        {
            let mut completions = td.completed_config_queries.lock().unwrap();
            completions.clear();
            completions.push((token, net_query));
        }
        td.drain_config_net_query_completions();
        assert!(td.completed_config_queries.lock().unwrap().is_empty());
        assert_eq!(td.config_manager.as_ref().unwrap().config_sent_cnt(), 0);

        td.close_impl(false);
        assert!(td.completed_config_queries.lock().unwrap().is_empty());
        assert!(td.sent_net_queries.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// getOption for the content-settings names routes through the
    /// ConfigManager and answers from the store immediately while
    /// unauthorized (the C++ `get_content_settings` Unit shortcut).
    #[test]
    fn get_option_content_settings_answers_in_run() {
        let callback = CollectCallback::default();
        let dir = temp_db_dir("content-get");
        let db_dir = dir.clone();
        run_actor_with(&callback, move |actor, callback, scheduler| {
            send(actor, scheduler, 70, set_tdlib_parameters(db_dir, false));
            assert!(
                responses_of(callback, 70)
                    .iter()
                    .any(|object| matches!(object, Object::ok(_)))
            );

            for name in [
                "ignore_sensitive_content_restrictions",
                "can_ignore_sensitive_content_restrictions",
            ] {
                send(
                    actor,
                    scheduler,
                    71,
                    Function::getOption(td_api::getOption {
                        name: name.to_string(),
                    }),
                );
                assert!(
                    responses_of(callback, 71)
                        .iter()
                        .any(|object| matches!(object, Object::optionValueEmpty(_))),
                    "{name} answers the empty store value"
                );
            }
            scheduler.finish();
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// setOption("ignore_sensitive_content_restrictions") without the
    /// `can_ignore` permission fails with the C++ error.
    #[test]
    fn set_option_content_settings_requires_can_ignore() {
        let callback = CollectCallback::default();
        let dir = temp_db_dir("content-set");
        let db_dir = dir.clone();
        run_actor_with(&callback, move |actor, callback, scheduler| {
            send(actor, scheduler, 80, set_tdlib_parameters(db_dir, false));
            assert!(
                responses_of(callback, 80)
                    .iter()
                    .any(|object| matches!(object, Object::ok(_)))
            );

            send(
                actor,
                scheduler,
                81,
                Function::setOption(td_api::setOption {
                    name: "ignore_sensitive_content_restrictions".to_string(),
                    value: td_api::OptionValue::optionValueBoolean(td_api::optionValueBoolean {
                        value: true,
                    }),
                }),
            );
            let error = error_of(callback, 81);
            assert_eq!(
                (error.code, error.message.as_str()),
                (
                    400,
                    "Option \"ignore_sensitive_content_restrictions\" can't be changed by the user"
                )
            );
            scheduler.finish();
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// getApplicationConfig queues the app-config fetch; feeding the result
    /// answers the request with the converted JSON object.
    #[test]
    fn get_application_config_answers_the_app_config() {
        let callback = CollectCallback::default();
        let dir = temp_db_dir("app-config");
        let db_dir = dir.clone();
        run_actor_with(&callback, move |actor, callback, scheduler| {
            send(actor, scheduler, 90, set_tdlib_parameters(db_dir, false));
            assert!(
                responses_of(callback, 90)
                    .iter()
                    .any(|object| matches!(object, Object::ok(_)))
            );

            send(
                actor,
                scheduler,
                91,
                Function::getApplicationConfig(td_api::getApplicationConfig),
            );
            // The query is pending — no response yet.
            assert!(responses_of(callback, 91).is_empty());

            let config = Box::new(telegram_api::JSONValue::jsonObject(
                telegram_api::jsonObject {
                    value: vec![Box::new(telegram_api::JSONObjectValue {
                        key: "k".to_string(),
                        value: Box::new(telegram_api::JSONValue::jsonNumber(
                            telegram_api::jsonNumber { value: 1.5 },
                        )),
                    })],
                },
            ));
            send_config_result(
                actor,
                scheduler,
                ConfigQuery::GetAppConfig { hash: 0 },
                ConfigQueryResult::GetAppConfig(Ok(
                    crate::config_manager::AppConfigQueryResult::Config { hash: 7, config },
                )),
            );
            assert!(responses_of(callback, 91).iter().any(
                |object| matches!(object, Object::jsonValueObject(v) if v.members.len() == 1)
            ));
            scheduler.finish();
        });
        let _ = std::fs::remove_dir_all(&dir);
    }
}
