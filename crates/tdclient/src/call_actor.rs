//! `CallActor` (td/telegram/CallActor.{h,cpp}) — the M.32 Wave-3 call
//! state machine. The C++ actor collapses to a plain struct (the M.13
//! convention); the net queries (request/accept/confirm/discard/received/
//! getCallConfig/getDhConfig) ride the manager-held `CallNetKind` plane:
//! the actor builds the TL at the send site and hands it to the env seam,
//! the `CallEnv` override dispatches, and the answers drain through
//! `Td::on_call_net_answer` (the promises park in the manager's FIFO
//! family lots like the C++ carrying them on the query). The call/media
//! stack DEP is stale at the pin: the eight wire families landed with
//! this plane while the media payload plane (tgcalls) stays parked —
//! the ready state carries the key/config the media stack would consume.
//! The actor timeouts (set_timeout_in) are recorded no-ops until the
//! actor envelope and Td timeout slot land, and the in-flight-query
//! cancel on discard is dropped with the queries themselves.

use crate::call_discard_reason::{
    CallDiscardReason, CallDiscardReasonType, get_call_discard_reason,
    get_group_call_invite_link_slug, get_input_phone_call_discard_reason,
};
use crate::call_id::CallId;
use crate::call_protocol::{CallConnection, CallProtocol, CallState, CallStateType};
use crate::chat_manager::DialogId;
use crate::dh_cache::{DhCache, DhCacheEnv, DhConfig};
use crate::misc::get_emoji_fingerprints;
use crate::store_parse::{self, Parser};
use crate::td_api;
use crate::user_manager::UserId;
use tdmtproto::dh_handshake::DhHandshake;
use tdmtproto::telegram_api;
use tdutils::crypto::sha256;
use tdutils::promise::Promise;
use tdutils::status::Status;

/// `phoneCallWaiting::RECEIVE_DATE_MASK` — the flags.1 bit of the TL
/// (`flags:# video:flags.0?true receive_date:flags.1?true`).
const RECEIVE_DATE_MASK: i32 = 1 << 1;

/// The `G()`/`td_` surface of the actor.
pub trait CallActorEnv: DhCacheEnv {
    /// `G()->close_flag()`.
    fn close_flag(&self) -> bool;

    /// `G()->get_option_integer`.
    fn get_option_integer(&self, name: &str, default_value: i64) -> i64;

    /// `G()->get_dh_config()` — the config the ConfigManager carries;
    /// `None` until one is fetched (the store write lives in the Td
    /// router's fetch lambda, `G()->set_dh_config()` analog,
    /// CallActor.cpp:691).
    fn get_dh_config(&self) -> Option<DhConfig>;

    /// `UserManager::on_get_users` — the phone_phoneCall user intake.
    fn on_get_users(&mut self, users: Vec<telegram_api::User>, source: &str);

    /// `UserManager::get_input_user` — the collapsed actor hop.
    fn get_input_user(
        &self,
        user_id: UserId,
    ) -> std::result::Result<telegram_api::InputUser, Status>;

    /// `UserManager::get_user_id_object_async` — the collapsed sync form
    /// (M.4.11's `get_user_id_object`); the update carries the resolved
    /// user id.
    fn get_user_id_object(&mut self, user_id: UserId) -> i64;

    /// `NotificationManager::add_call_notification` (cpp:2391-2420).
    fn add_call_notification(&mut self, dialog_id: DialogId, call_id: CallId);
    /// `NotificationManager::remove_call_notification` (cpp:2422-2471).
    fn remove_call_notification(&mut self, dialog_id: DialogId, call_id: CallId);

    /// `send_closure(G()->td(), &Td::send_update, ...)`.
    fn send_update(&mut self, update: td_api::Update);

    /// `getCallConfig` (start_up, cpp:884-891) — the dispatch seam; the
    /// actor builds the TL and the `CallEnv` override rides the
    /// `CallNetKind` plane (the offline default logs the net-inert
    /// posture). The answer handler is [`CallActor::on_get_call_config_result`].
    fn send_get_call_config_query(&mut self, call_id: CallId, function: telegram_api::Function) {
        let _ = (call_id, function);
        tdutils::log_info!("Send phone.getCallConfig — [net-inert]");
    }

    /// `messages_getDhConfig` (do_load_dh_config, cpp:668-703) — the
    /// answer parses in the Td router (the DhConfig build + the DRBG
    /// seed + the stored-config fallback, cpp:683-701) and lands in
    /// [`CallActor::on_dh_config`].
    fn send_get_dh_config_query(&mut self, call_id: CallId, function: telegram_api::Function) {
        let _ = (call_id, function);
        tdutils::log_info!("Send messages.getDhConfig — [net-inert]");
    }

    /// `phone_requestCall` (try_send_request_query, cpp:734-736) — the
    /// answer rides [`CallActor::update_call_inner`] (cpp:753-758); the
    /// C++ `total_timeout_limit_` + `request_query_ref_` ride the fixed
    /// dispatch budget (no cancel surface — the standing divergence).
    fn send_request_call_query(&mut self, call_id: CallId, function: telegram_api::Function) {
        let _ = (call_id, function);
        tdutils::log_info!("Send phone.requestCall — [net-inert]");
    }

    /// `phone_acceptCall` (cpp:772-774) — the answer rides
    /// [`CallActor::update_call_inner`] (cpp:784-789).
    fn send_accept_call_query(&mut self, call_id: CallId, function: telegram_api::Function) {
        let _ = (call_id, function);
        tdutils::log_info!("Send phone.acceptCall — [net-inert]");
    }

    /// `phone_confirmCall` (cpp:797-799) — the answer rides
    /// [`CallActor::update_call_inner`] (cpp:808-812).
    fn send_confirm_call_query(&mut self, call_id: CallId, function: telegram_api::Function) {
        let _ = (call_id, function);
        tdutils::log_info!("Send phone.confirmCall — [net-inert]");
    }

    /// `phone_discardCall` (cpp:826-828) — the ok answer rides the
    /// Updates engine (cpp:837-841), the fetch error routes into
    /// [`CallActor::on_error`].
    fn send_discard_call_query(&mut self, call_id: CallId, function: telegram_api::Function) {
        let _ = (call_id, function);
        tdutils::log_info!("Send phone.discardCall — [net-inert]");
    }

    /// `phone_receivedCall` (cpp:706) — the ok answer is unused
    /// (cpp:714-719), the fetch error routes into [`CallActor::on_error`].
    fn send_received_call_query(&mut self, call_id: CallId, function: telegram_api::Function) {
        let _ = (call_id, function);
        tdutils::log_info!("Send phone.receivedCall — [net-inert]");
    }

    /// `phone_sendSignalingData` (cpp:278-279) — the request promise
    /// parks in the manager's FIFO family lot and settles in the answer
    /// router (cpp:280-289); the actor never sees it.
    fn send_call_signaling_data_query(
        &mut self,
        call_id: CallId,
        function: telegram_api::Function,
    ) {
        let _ = (call_id, function);
        tdutils::log_info!("Send phone.sendSignalingData — [net-inert]");
    }
}

/// `CallActor::State` (CallActor.h:118-131).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CallActorState {
    #[default]
    Empty,
    SendRequestQuery,
    WaitRequestResult,
    SendAcceptQuery,
    WaitAcceptResult,
    SendConfirmQuery,
    WaitConfirmResult,
    Ready,
    SendDiscardQuery,
    WaitDiscardResult,
    Discarded,
}

/// `CallFullId` (CallActor.cpp:36-55) — the recent-call pmc record.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CallFullId {
    call_id: i64,
    access_hash: i64,
}

impl CallFullId {
    fn store(&self, out: &mut Vec<u8>) {
        store_parse::store_i64(out, self.call_id);
        store_parse::store_i64(out, self.access_hash);
    }

    fn parse(parser: &mut Parser) -> Result<CallFullId, &'static str> {
        Ok(CallFullId {
            call_id: parser.i64_()?,
            access_hash: parser.i64_()?,
        })
    }
}

/// `load_recent_call_full_ids` (CallActor.cpp:57-64).
fn load_recent_call_full_ids(env: &mut impl CallActorEnv) -> Vec<CallFullId> {
    let log_event_string = env.pmc_get_raw(b"recent_call_ids");
    if log_event_string.is_empty() {
        return Vec::new();
    }
    // `log_event_parse(...).ensure()` — the C++ aborts on a corrupt blob.
    let mut parser = Parser::new_log_event(&log_event_string).expect("recent_call_ids log event");
    let count = parser.i32_().expect("recent_call_ids vector size");
    let mut result = Vec::with_capacity(count as usize);
    for _ in 0..count {
        result.push(CallFullId::parse(&mut parser).expect("recent_call_ids entry"));
    }
    result
}

/// `save_recent_call_full_ids` (CallActor.cpp:66-68).
fn save_recent_call_full_ids(env: &mut impl CallActorEnv, recent_call_full_ids: &[CallFullId]) {
    let mut data = Vec::new();
    store_parse::store_log_event_version(&mut data);
    store_parse::store_i32(&mut data, recent_call_full_ids.len() as i32);
    for call_full_id in recent_call_full_ids {
        call_full_id.store(&mut data);
    }
    env.pmc_set_raw(b"recent_call_ids", &data);
}

/// The collapsed `CallActor`.
pub struct CallActor {
    pub local_call_id: CallId,
    pub state: CallActorState,
    is_outgoing: bool,
    is_accepted: bool,
    is_video: bool,
    pub user_id: UserId,
    pub call_admin_user_id: UserId,
    /// `call_id_` — the server call id.
    pub call_id: i64,
    call_access_hash: i64,
    is_call_id_inited: bool,
    is_call_id_saved: bool,
    has_notification: bool,
    call_state_has_config: bool,
    duration: i32,
    connection_id: i64,
    call_state: CallState,
    call_state_need_flush: bool,
    call_id_promise: Option<Promise<i64>>,
    dh_handshake: DhHandshake,
    dh_config: Option<DhConfig>,
    dh_config_query_sent: bool,
}

impl CallActor {
    /// `CallActor()` (CallActor.cpp:222-224).
    pub fn new(local_call_id: CallId) -> CallActor {
        CallActor {
            local_call_id,
            state: CallActorState::Empty,
            is_outgoing: false,
            is_accepted: false,
            is_video: false,
            user_id: UserId::default(),
            call_admin_user_id: UserId::default(),
            call_id: 0,
            call_access_hash: 0,
            is_call_id_inited: false,
            is_call_id_saved: false,
            has_notification: false,
            call_state_has_config: false,
            duration: 0,
            connection_id: 0,
            call_state: CallState::default(),
            call_state_need_flush: false,
            call_id_promise: None,
            dh_handshake: DhHandshake::new(),
            dh_config: None,
            dh_config_query_sent: false,
        }
    }

    /// `get_recent_call_access_hash` (CallActor.cpp:226-234).
    pub fn get_recent_call_access_hash(env: &mut impl CallActorEnv, call_id: i64) -> i64 {
        for call_full_id in load_recent_call_full_ids(env) {
            if call_full_id.call_id == call_id {
                return call_full_id.access_hash;
            }
        }
        0
    }

    /// `create_call` (CallActor.cpp:236-248) — returns the local call id
    /// for the td_api answer (the C++ `promise.set_value(CallId(...))`).
    pub fn create_call(
        &mut self,
        env: &mut impl CallActorEnv,
        user_id: UserId,
        protocol: CallProtocol,
        is_video: bool,
    ) -> CallId {
        debug_assert_eq!(self.state, CallActorState::Empty);
        self.state = CallActorState::SendRequestQuery;
        self.is_outgoing = true;
        self.is_video = is_video;
        self.user_id = user_id;
        self.call_state.protocol = protocol;
        self.call_state.type_ = CallStateType::Pending;
        self.call_state.is_received = false;
        self.call_state_need_flush = true;
        self.loop_(env);
        self.local_call_id
    }

    /// `accept_call` (CallActor.cpp:250-258).
    pub fn accept_call(
        &mut self,
        env: &mut impl CallActorEnv,
        protocol: CallProtocol,
        mut promise: Promise<()>,
    ) {
        if self.state != CallActorState::SendAcceptQuery {
            promise.set_error(Status::error(400, "Unexpected acceptCall"));
            return;
        }
        self.is_accepted = true;
        self.call_state.protocol = protocol;
        promise.set_value(());
        self.loop_(env);
    }

    /// `update_call_signaling_data` (CallActor.cpp:260-270).
    pub fn update_call_signaling_data(&mut self, env: &mut impl CallActorEnv, data: Vec<u8>) {
        if self.call_state.type_ != CallStateType::Ready
            && self.call_state.discard_reason.type_ != CallDiscardReasonType::UpgradeToGroupCall
        {
            return;
        }
        env.send_update(td_api::Update::updateNewCallSignalingData(
            td_api::updateNewCallSignalingData {
                call_id: self.local_call_id.get(),
                data,
            },
        ));
    }

    /// `send_call_signaling_data` (CallActor.cpp:272-289) — the state
    /// gate + the TL build; the manager parks the request promise in the
    /// FIFO family lot and dispatches (the C++ promise rides the query
    /// lambda, cpp:280-289).
    pub fn send_call_signaling_data(
        &mut self,
        data: Vec<u8>,
    ) -> std::result::Result<telegram_api::Function, Status> {
        if self.call_state.type_ != CallStateType::Ready
            && self.call_state.discard_reason.type_ != CallDiscardReasonType::UpgradeToGroupCall
        {
            return Err(Status::error(400, "Call is not active"));
        }
        Ok(telegram_api::Function::phone_sendSignalingData(
            telegram_api::phone_sendSignalingData {
                peer: Some(self.get_input_phone_call("send_call_signaling_data")),
                data,
            },
        ))
    }

    /// `discard_call` (CallActor.cpp:291-346) — the C++ signature shape.
    #[allow(clippy::too_many_arguments)]
    pub fn discard_call(
        &mut self,
        env: &mut impl CallActorEnv,
        is_disconnected: bool,
        invite_link: &str,
        duration: i32,
        is_video: bool,
        connection_id: i64,
        mut promise: Promise<()>,
    ) {
        if self.state == CallActorState::Discarded
            || self.state == CallActorState::WaitDiscardResult
            || self.state == CallActorState::SendDiscardQuery
        {
            promise.set_value(());
            return;
        }

        match self.call_state.type_ {
            CallStateType::Empty | CallStateType::Pending => {
                self.call_state.discard_reason.type_ = if self.is_outgoing {
                    CallDiscardReasonType::Missed
                } else {
                    CallDiscardReasonType::Declined
                };
            }
            CallStateType::ExchangingKey => {
                self.call_state.discard_reason.type_ = if is_disconnected {
                    CallDiscardReasonType::Disconnected
                } else {
                    CallDiscardReasonType::HungUp
                };
            }
            CallStateType::Ready => {
                if !invite_link.is_empty() {
                    let slug = get_group_call_invite_link_slug(invite_link);
                    if slug.is_empty() {
                        promise.set_error(Status::error(400, "Invalid invite link specified"));
                        return;
                    }
                    self.call_state.discard_reason.type_ =
                        CallDiscardReasonType::UpgradeToGroupCall;
                    self.call_state.discard_reason.slug = slug;
                } else {
                    self.call_state.discard_reason.type_ = if is_disconnected {
                        CallDiscardReasonType::Disconnected
                    } else {
                        CallDiscardReasonType::HungUp
                    };
                }
                self.duration = duration;
                self.connection_id = connection_id;
            }
            _ => unreachable!("discard_call from the hanging-up family"),
        }

        self.is_video |= is_video;

        self.call_state.type_ = CallStateType::HangingUp;
        self.call_state_need_flush = true;

        self.state = CallActorState::SendDiscardQuery;
        promise.set_value(());
        self.loop_(env);
    }

    /// `get_input_phone_call_to_promise` (CallActor.cpp:348-354).
    pub fn get_input_phone_call_to_promise(
        &self,
        mut promise: Promise<telegram_api::InputPhoneCall>,
    ) {
        if !self.is_call_id_inited {
            promise.set_error(Status::error(400, "Call not found"));
            return;
        }
        promise.set_value(self.get_input_phone_call("get_input_phone_call_to_promise"));
    }

    /// `on_set_call_rating` (CallActor.cpp:356-362).
    pub fn on_set_call_rating(&mut self, env: &mut impl CallActorEnv) {
        if self.call_state.need_rating {
            self.call_state.need_rating = false;
            self.call_state_need_flush = true;
            self.loop_(env);
        }
    }

    /// `on_save_debug_information` (CallActor.cpp:364-376).
    pub fn on_save_debug_information(&mut self, env: &mut impl CallActorEnv, result: bool) {
        if !result && !self.call_state.need_log {
            self.call_state.need_log = true;
            self.call_state_need_flush = true;
        }
        if self.call_state.need_debug_information {
            self.call_state.need_debug_information = false;
            self.call_state_need_flush = true;
        }
        if self.call_state_need_flush {
            self.loop_(env);
        }
    }

    /// `on_save_log` (CallActor.cpp:378-384).
    pub fn on_save_log(&mut self, env: &mut impl CallActorEnv) {
        if self.call_state.need_log {
            self.call_state.need_log = false;
            self.call_state_need_flush = true;
            self.loop_(env);
        }
    }

    /// `update_call` (CallActor.cpp:386-412).
    pub fn update_call(&mut self, env: &mut impl CallActorEnv, call: telegram_api::PhoneCall) {
        tdutils::log_info!("Receive {call:?}");
        let status = match call.clone() {
            telegram_api::PhoneCall::phoneCallEmpty(call) => self.do_update_call_empty(env, call),
            telegram_api::PhoneCall::phoneCallWaiting(call) => {
                self.do_update_call_waiting(env, call)
            }
            telegram_api::PhoneCall::phoneCallRequested(call) => {
                self.do_update_call_requested(env, call)
            }
            telegram_api::PhoneCall::phoneCallAccepted(call) => {
                self.do_update_call_accepted(env, call)
            }
            telegram_api::PhoneCall::phoneCall(call) => self.do_update_call_ready(env, call),
            telegram_api::PhoneCall::phoneCallDiscarded(call) => {
                self.do_update_call_discarded(env, call)
            }
        };
        if let Err(status) = status {
            tdutils::log_info!("Receive error {status}, while handling update {call:?}");
            self.on_error(env, status);
        }
        self.loop_(env);
    }

    /// `update_call_inner` (CallActor.cpp:414-418) — the phone.phoneCall
    /// split (users → user manager, call → the state machine).
    pub fn update_call_inner(
        &mut self,
        env: &mut impl CallActorEnv,
        call: telegram_api::phone_PhoneCall,
    ) {
        env.on_get_users(call.users, "UpdatePhoneCall");
        self.update_call(
            env,
            call.phone_call.expect("phone.phoneCall carries a call"),
        );
    }

    /// `get_input_phone_call` (CallActor.cpp:489-492).
    fn get_input_phone_call(&self, source: &str) -> telegram_api::InputPhoneCall {
        debug_assert!(self.is_call_id_inited, "{source}");
        telegram_api::InputPhoneCall {
            id: self.call_id,
            access_hash: self.call_access_hash,
        }
    }

    fn do_update_call_empty(
        &mut self,
        _env: &mut impl CallActorEnv,
        _call: telegram_api::phoneCallEmpty,
    ) -> std::result::Result<(), Status> {
        Err(Status::error(400, "Call is finished"))
    }

    fn do_update_call_waiting(
        &mut self,
        env: &mut impl CallActorEnv,
        call: telegram_api::phoneCallWaiting,
    ) -> std::result::Result<(), Status> {
        if self.state != CallActorState::WaitRequestResult
            && self.state != CallActorState::WaitAcceptResult
        {
            return Ok(());
        }

        if self.state == CallActorState::WaitAcceptResult {
            self.on_begin_exchanging_key();
        } else if (call.flags & RECEIVE_DATE_MASK) != 0 {
            if !self.call_state.is_received {
                self.call_state.is_received = true;
                self.call_state_need_flush = true;
            }
            // set_timeout_in(call_ring_timeout_ms) — the recorded
            // actor-timeout collapse (the envelope lands with the net step).
            let _ = env.get_option_integer("call_ring_timeout_ms", 90000);
        }

        if self.call_id != call.id {
            self.call_id = call.id;
            self.call_state_need_flush = true;
        }
        self.call_access_hash = call.access_hash;
        self.is_call_id_inited = true;
        self.call_admin_user_id = UserId::new(call.admin_id);
        self.on_get_call_id(env);

        if call.video && !self.is_video {
            self.is_video = true;
            self.call_state_need_flush = true;
        }
        if !self.call_state.is_created {
            self.call_state.is_created = true;
            self.call_state_need_flush = true;
        }

        Ok(())
    }

    fn do_update_call_requested(
        &mut self,
        env: &mut impl CallActorEnv,
        call: telegram_api::phoneCallRequested,
    ) -> std::result::Result<(), Status> {
        if self.state != CallActorState::Empty {
            return Ok(());
        }
        self.call_id = call.id;
        self.call_access_hash = call.access_hash;
        self.is_call_id_inited = true;
        self.is_video |= call.video;
        self.call_admin_user_id = UserId::new(call.admin_id);
        self.on_get_call_id(env);

        self.dh_handshake.set_g_a_hash(&call.g_a_hash);
        self.state = CallActorState::SendAcceptQuery;

        self.call_state.type_ = CallStateType::Pending;
        self.call_state.is_created = true;
        self.call_state.is_received = true;
        self.call_state_need_flush = true;

        self.send_received_query(env);
        Ok(())
    }

    fn do_update_call_accepted(
        &mut self,
        env: &mut impl CallActorEnv,
        call: telegram_api::phoneCallAccepted,
    ) -> std::result::Result<(), Status> {
        if self.state != CallActorState::WaitRequestResult {
            return Ok(());
        }

        if !self.is_call_id_inited {
            self.call_id = call.id;
            self.call_access_hash = call.access_hash;
            self.is_call_id_inited = true;
            self.call_admin_user_id = UserId::new(call.admin_id);
            self.on_get_call_id(env);
        }
        self.is_video |= call.video;
        self.dh_handshake.set_g_a(&call.g_b);
        let status = self.dh_handshake.run_checks(true, &DhCache::instance());
        if status.is_error() {
            return Err(status);
        }
        let (key_fingerprint, key) = self.dh_handshake.gen_key();
        self.call_state.key_fingerprint = key_fingerprint;
        self.call_state.key = key;
        self.state = CallActorState::SendConfirmQuery;
        self.on_begin_exchanging_key();
        Ok(())
    }

    fn do_update_call_ready(
        &mut self,
        _env: &mut impl CallActorEnv,
        call: telegram_api::phoneCall,
    ) -> std::result::Result<(), Status> {
        if self.state != CallActorState::WaitAcceptResult
            && self.state != CallActorState::WaitConfirmResult
        {
            return Ok(());
        }

        if self.state == CallActorState::WaitAcceptResult {
            self.dh_handshake.set_g_a(&call.g_a_or_b);
            let status = self.dh_handshake.run_checks(true, &DhCache::instance());
            if status.is_error() {
                return Err(status);
            }
            let (key_fingerprint, key) = self.dh_handshake.gen_key();
            self.call_state.key_fingerprint = key_fingerprint;
            self.call_state.key = key;
        }
        if self.call_state.key_fingerprint != call.key_fingerprint {
            return Err(Status::error(400, "Key fingerprints mismatch"));
        }

        self.state = CallActorState::Ready;
        self.is_video |= call.video;
        self.call_state.emojis_fingerprint = Self::get_emojis_fingerprint(
            &self.call_state.key,
            if self.is_outgoing {
                self.dh_handshake.get_g_b()
            } else {
                self.dh_handshake.get_g_a()
            },
        );

        for connection in &call.connections {
            self.call_state
                .connections
                .push(CallConnection::from_telegram(connection));
        }
        self.call_state.protocol = CallProtocol::from_telegram(
            call.protocol
                .as_ref()
                .expect("phoneCall always carries a protocol"),
        );
        self.call_state.allow_p2p = call.p2p_allowed;
        self.call_state.conference_supported = call.conference_supported;
        if let Some(custom_parameters) = &call.custom_parameters {
            self.call_state.custom_parameters = custom_parameters.data.clone();
        }
        self.call_state.type_ = CallStateType::Ready;
        self.call_state_need_flush = true;
        Ok(())
    }

    fn do_update_call_discarded(
        &mut self,
        _env: &mut impl CallActorEnv,
        call: telegram_api::phoneCallDiscarded,
    ) -> std::result::Result<(), Status> {
        self.on_call_discarded(
            get_call_discard_reason(&call.reason),
            call.need_rating,
            call.need_debug,
            call.video,
        );
        Ok(())
    }

    /// `on_get_call_id` (CallActor.cpp:567-590).
    fn on_get_call_id(&mut self, env: &mut impl CallActorEnv) {
        if let Some(mut call_id_promise) = self.call_id_promise.take() {
            call_id_promise.set_value(self.call_id);
        }
        if !self.is_call_id_saved {
            let mut recent_call_full_ids = load_recent_call_full_ids(env);
            let is_found = recent_call_full_ids
                .iter()
                .any(|call_full_id| call_full_id.call_id == self.call_id);
            if !is_found {
                recent_call_full_ids.push(CallFullId {
                    call_id: self.call_id,
                    access_hash: self.call_access_hash,
                });
                if recent_call_full_ids.len() > 10 {
                    recent_call_full_ids.remove(0);
                }
                save_recent_call_full_ids(env, &recent_call_full_ids);
            }
            self.is_call_id_saved = true;
        }
    }

    /// `on_call_discarded` (CallActor.cpp:592-614).
    fn on_call_discarded(
        &mut self,
        reason: CallDiscardReason,
        need_rating: bool,
        need_debug: bool,
        is_video: bool,
    ) {
        if self.state != CallActorState::Discarded {
            self.state = CallActorState::Discarded;
            self.call_state_need_flush = true;
        }
        if is_video && !self.is_video {
            self.is_video = true;
            self.call_state_need_flush = true;
        }
        if self.call_state.discard_reason != reason && reason.type_ != CallDiscardReasonType::Empty
        {
            self.call_state.discard_reason = reason;
            self.call_state_need_flush = true;
        }
        if self.call_state.type_ != CallStateType::Error
            && (self.call_state.need_rating != need_rating
                || self.call_state.need_debug_information != need_debug
                || self.call_state.type_ != CallStateType::Discarded)
        {
            self.call_state.need_rating = need_rating;
            self.call_state.need_debug_information = need_debug;
            self.call_state.type_ = CallStateType::Discarded;
            self.call_state_need_flush = true;
        }
    }

    /// The `on_dh_config` good-prime gate (CallActor.cpp:655-662): the
    /// config folds into the actor member (cpp:655) and a bad prime
    /// fails the call through `on_error` (cpp:660-662). Every config
    /// passes through it — the fetch answer directly, the cached store
    /// read over the NotModified fallback (cpp:683-701).
    fn take_dh_config_checked(&mut self, env: &mut impl CallActorEnv, config: DhConfig) -> bool {
        let status = DhHandshake::check_config(config.g, &config.prime, &DhCache::instance());
        self.dh_config = Some(config);
        if status.is_error() {
            self.on_error(env, status);
            return false;
        }
        true
    }

    /// `load_dh_config` (CallActor.cpp:616-629). The cached branch is
    /// the port's store read (the `G()->get_dh_config()` analog) — the
    /// C++ has no cached return here; its cached config rides the
    /// `dhConfigNotModified` answer back through `on_dh_config`'s check
    /// (cpp:699-701), so the read passes the same gate.
    fn load_dh_config(&mut self, env: &mut impl CallActorEnv) -> bool {
        if let Some(config) = env.get_dh_config()
            && !self.take_dh_config_checked(env, config)
        {
            // The `update_call` on_error convention — the loop pass
            // flushes the error state and rides the local discard
            // tail (the C++ re-yields the actor loop, cpp:660-662).
            self.loop_(env);
            return false;
        }
        if self.dh_config.is_some() {
            return true;
        }
        if !self.dh_config_query_sent {
            self.dh_config_query_sent = true;
            self.do_load_dh_config(env);
        }
        false
    }

    /// `on_error` (CallActor.cpp:631-651).
    pub fn on_error(&mut self, _env: &mut impl CallActorEnv, status: Status) {
        tdutils::log_info!("Receive error {status}");

        if self.state == CallActorState::WaitDiscardResult
            || self.state == CallActorState::Discarded
        {
            self.state = CallActorState::Discarded;
        } else {
            self.state = CallActorState::SendDiscardQuery;
            self.call_state.discard_reason.type_ =
                if self.call_state.type_ == CallStateType::Pending {
                    CallDiscardReasonType::Missed
                } else {
                    CallDiscardReasonType::Disconnected
                };
        }

        self.call_state.type_ = CallStateType::Error;
        self.call_state.error = Some(td_api::Error {
            code: status.code(),
            message: status.message().to_string(),
        });
        self.call_state_need_flush = true;
    }

    /// `on_dh_config` (CallActor.cpp:653-666) — the C++ fetch lambda
    /// (the Td router) builds + stores the config (cpp:691); this tail
    /// takes it into the actor member (`dh_config_ = move_as_ok()`,
    /// cpp:655) and runs the good-prime check.
    pub fn on_dh_config(
        &mut self,
        env: &mut impl CallActorEnv,
        result: std::result::Result<DhConfig, Status>,
    ) {
        let config = match result {
            Ok(config) => config,
            Err(status) => return self.on_error(env, status),
        };
        if !self.take_dh_config_checked(env, config) {
            return;
        }
        self.dh_config_query_sent = false;
        self.loop_(env);
    }

    /// `do_load_dh_config` (CallActor.cpp:668-703) — the answer parses
    /// in the Td router (the DhConfig build + the DRBG seed + the
    /// stored-config fallback, cpp:683-701) and lands in
    /// [`CallActor::on_dh_config`].
    fn do_load_dh_config(&mut self, env: &mut impl CallActorEnv) {
        let version = env.get_dh_config().map_or(0, |config| config.version);
        env.send_get_dh_config_query(
            self.local_call_id,
            telegram_api::Function::messages_getDhConfig(telegram_api::messages_getDhConfig {
                version,
                random_length: 0,
            }),
        );
    }

    /// `send_received_query` (CallActor.cpp:705-712).
    fn send_received_query(&mut self, env: &mut impl CallActorEnv) {
        env.send_received_call_query(
            self.local_call_id,
            telegram_api::Function::phone_receivedCall(telegram_api::phone_receivedCall {
                peer: Some(self.get_input_phone_call("send_received_query")),
            }),
        );
    }

    /// `try_send_request_query` (CallActor.cpp:721-751) — the DH halves
    /// are real and the TL rides the dispatch seam; the C++
    /// `total_timeout_limit_` rides the fixed 24h dispatch budget here
    /// and the `request_query_ref_` cancel is dropped (the standing
    /// slice divergences, cpp:743-746).
    fn try_send_request_query(&mut self, env: &mut impl CallActorEnv) {
        if env.close_flag() {
            return;
        }
        if !self.load_dh_config(env) {
            return;
        }
        let dh_config = self.dh_config.clone().expect("dh config just loaded");
        self.dh_handshake.set_config(dh_config.g, &dh_config.prime);
        let input_user = env.get_input_user(self.user_id);
        let Ok(input_user) = input_user else {
            return self.on_error(env, input_user.unwrap_err());
        };
        self.state = CallActorState::WaitRequestResult;
        // the ring timeout is the recorded actor-timeout collapse
        let _ = env.get_option_integer("call_receive_timeout_ms", 20000);
        env.send_request_call_query(
            self.local_call_id,
            telegram_api::Function::phone_requestCall(telegram_api::phone_requestCall {
                flags: 0,
                video: self.is_video,
                user_id: Some(input_user),
                random_id: tdutils::random::secure_int32(),
                g_a_hash: self.dh_handshake.get_g_b_hash().to_vec(),
                protocol: Some(self.call_state.protocol.get_input_phone_call_protocol()),
            }),
        );
    }

    /// `try_send_accept_query` (CallActor.cpp:762-781).
    fn try_send_accept_query(&mut self, env: &mut impl CallActorEnv) {
        if !self.load_dh_config(env) {
            return;
        }
        if !self.is_accepted {
            return;
        }
        let dh_config = self.dh_config.clone().expect("dh config just loaded");
        self.dh_handshake.set_config(dh_config.g, &dh_config.prime);
        let function = telegram_api::Function::phone_acceptCall(telegram_api::phone_acceptCall {
            peer: Some(self.get_input_phone_call("try_send_accept_query")),
            g_b: self.dh_handshake.get_g_b(),
            protocol: Some(self.call_state.protocol.get_input_phone_call_protocol()),
        });
        self.state = CallActorState::WaitAcceptResult;
        env.send_accept_call_query(self.local_call_id, function);
    }

    /// `try_send_confirm_query` (CallActor.cpp:792-806) — the TL field
    /// is named `g_a` but carries `g_b` (cpp:797-799).
    fn try_send_confirm_query(&mut self, env: &mut impl CallActorEnv) {
        if !self.load_dh_config(env) {
            return;
        }
        let function = telegram_api::Function::phone_confirmCall(telegram_api::phone_confirmCall {
            peer: Some(self.get_input_phone_call("try_send_confirm_query")),
            g_a: self.dh_handshake.get_g_b(),
            key_fingerprint: self.call_state.key_fingerprint,
            protocol: Some(self.call_state.protocol.get_input_phone_call_protocol()),
        });
        self.state = CallActorState::WaitConfirmResult;
        env.send_confirm_call_query(self.local_call_id, function);
    }

    /// `try_send_discard_query` (CallActor.cpp:816-835).
    fn try_send_discard_query(&mut self, env: &mut impl CallActorEnv) {
        if self.call_id == 0 {
            let reason = CallDiscardReason {
                type_: CallDiscardReasonType::Missed,
                slug: String::new(),
            };
            self.on_call_discarded(reason, false, false, self.is_video);
            self.loop_(env);
            return;
        }
        let function = telegram_api::Function::phone_discardCall(telegram_api::phone_discardCall {
            flags: 0,
            video: self.is_video,
            peer: Some(self.get_input_phone_call("try_send_discard_query")),
            duration: self.duration,
            reason: get_input_phone_call_discard_reason(&self.call_state.discard_reason),
            connection_id: self.connection_id,
        });
        self.state = CallActorState::WaitDiscardResult;
        env.send_discard_call_query(self.local_call_id, function);
    }

    /// `on_begin_exchanging_key` (CallActor.cpp:516-523).
    fn on_begin_exchanging_key(&mut self) {
        self.call_state.type_ = CallStateType::ExchangingKey;
        self.call_state_need_flush = true;
        // the receive timeout is the recorded actor-timeout collapse
    }

    /// `flush_call_state` (CallActor.cpp:845-882).
    fn flush_call_state(&mut self, env: &mut impl CallActorEnv) {
        if env.close_flag() {
            return;
        }
        if self.call_state_need_flush {
            if !self.is_outgoing {
                if self.call_state.type_ == CallStateType::Pending {
                    if !self.has_notification {
                        self.has_notification = true;
                        env.add_call_notification(
                            DialogId::from_user_id(self.call_admin_user_id),
                            self.local_call_id,
                        );
                    }
                } else if self.has_notification {
                    self.has_notification = false;
                    env.remove_call_notification(
                        DialogId::from_user_id(self.call_admin_user_id),
                        self.local_call_id,
                    );
                }
            }

            if self.call_state.type_ == CallStateType::Ready && !self.call_state_has_config {
                return;
            }
            self.call_state_need_flush = false;

            let peer_id = if self.is_outgoing {
                self.user_id
            } else {
                self.call_admin_user_id
            };
            let user_id = env.get_user_id_object(peer_id);
            env.send_update(td_api::Update::updateCall(td_api::updateCall {
                call: Some(td_api::Call {
                    id: self.local_call_id.get(),
                    unique_id: self.call_id,
                    user_id,
                    is_outgoing: self.is_outgoing,
                    is_video: self.is_video,
                    state: Some(self.call_state.get_call_state_object()),
                }),
            }));
        }
    }

    /// `start_up` (CallActor.cpp:884-891).
    pub fn start_up(&mut self, env: &mut impl CallActorEnv) {
        env.send_get_call_config_query(
            self.local_call_id,
            telegram_api::Function::phone_getCallConfig(telegram_api::phone_getCallConfig),
        );
    }

    /// `on_get_call_config_result` (CallActor.cpp:893-900).
    pub fn on_get_call_config_result(
        &mut self,
        env: &mut impl CallActorEnv,
        result: std::result::Result<String, Status>,
    ) {
        match result {
            Err(status) => self.on_error(env, status),
            Ok(data) => {
                self.call_state.config = data;
                self.call_state_has_config = true;
                self.loop_(env);
            }
        }
    }

    /// The C++ `loop` stop condition for the `Discarded` state
    /// (CallActor.cpp:918-928): the actor closes once the discarded state
    /// owes no rating/debug/log bookkeeping.
    pub fn should_stop(&self) -> bool {
        self.state == CallActorState::Discarded
            && !(self.call_state.type_ == CallStateType::Discarded
                && (self.call_state.need_rating
                    || self.call_state.need_debug_information
                    || self.call_state.need_log
                    || self.call_state.discard_reason.type_
                        == CallDiscardReasonType::UpgradeToGroupCall))
    }

    /// `loop` (CallActor.cpp:902-933).
    pub fn loop_(&mut self, env: &mut impl CallActorEnv) {
        self.flush_call_state(env);
        match self.state {
            CallActorState::SendRequestQuery => self.try_send_request_query(env),
            CallActorState::SendAcceptQuery => self.try_send_accept_query(env),
            CallActorState::SendConfirmQuery => self.try_send_confirm_query(env),
            CallActorState::SendDiscardQuery => self.try_send_discard_query(env),
            CallActorState::Discarded => {
                // the C++ breaks the switch when the discarded state still
                // owes rating/debug/log bookkeeping and stops the actor
                // otherwise; the collapsed manager drops the entry on the
                // next sweep (see [`CallActor::should_stop`])
            }
            _ => {}
        }
    }

    /// `timeout_expired` (CallActor.cpp:935-938).
    pub fn timeout_expired(&mut self, env: &mut impl CallActorEnv) {
        self.on_error(env, Status::error(4005000, "Call timeout expired"));
        self.loop_(env);
    }

    /// `get_emojis_fingerprint` (CallActor.cpp:957-962).
    pub fn get_emojis_fingerprint(key: &[u8], g_a: Vec<u8>) -> Vec<String> {
        let mut data = key.to_vec();
        data.extend_from_slice(&g_a);
        get_emoji_fingerprints(&sha256(&data))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    struct TestEnv {
        pmc: BTreeMap<Vec<u8>, Vec<u8>>,
        dh_config: Option<DhConfig>,
        updates: Vec<td_api::Update>,
        queries: Vec<String>,
        user_id_object: i64,
    }

    impl TestEnv {
        fn new() -> TestEnv {
            TestEnv {
                pmc: BTreeMap::new(),
                dh_config: None,
                updates: Vec::new(),
                queries: Vec::new(),
                user_id_object: 7,
            }
        }
    }

    impl DhCacheEnv for TestEnv {
        fn pmc_get_raw(&mut self, key: &[u8]) -> Vec<u8> {
            self.pmc.get(key).cloned().unwrap_or_default()
        }

        fn pmc_set_raw(&mut self, key: &[u8], value: &[u8]) {
            self.pmc.insert(key.to_vec(), value.to_vec());
        }
    }

    impl CallActorEnv for TestEnv {
        fn close_flag(&self) -> bool {
            false
        }

        fn get_option_integer(&self, _name: &str, default_value: i64) -> i64 {
            default_value
        }

        fn get_dh_config(&self) -> Option<DhConfig> {
            self.dh_config.clone()
        }

        fn on_get_users(&mut self, _users: Vec<telegram_api::User>, _source: &str) {}

        fn get_input_user(
            &self,
            _user_id: UserId,
        ) -> std::result::Result<telegram_api::InputUser, Status> {
            Ok(telegram_api::InputUser::inputUser(
                telegram_api::inputUser {
                    user_id: 5,
                    access_hash: 9,
                },
            ))
        }

        fn get_user_id_object(&mut self, _user_id: UserId) -> i64 {
            self.user_id_object
        }

        fn add_call_notification(&mut self, _dialog_id: DialogId, _call_id: CallId) {}

        fn remove_call_notification(&mut self, _dialog_id: DialogId, _call_id: CallId) {}

        fn send_update(&mut self, update: td_api::Update) {
            self.updates.push(update);
        }

        fn send_get_call_config_query(
            &mut self,
            _call_id: CallId,
            _function: telegram_api::Function,
        ) {
            self.queries.push("getCallConfig".to_string());
        }

        fn send_get_dh_config_query(
            &mut self,
            _call_id: CallId,
            _function: telegram_api::Function,
        ) {
            self.queries.push("getDhConfig".to_string());
        }

        fn send_request_call_query(&mut self, _call_id: CallId, _function: telegram_api::Function) {
            self.queries.push("requestCall".to_string());
        }

        fn send_accept_call_query(&mut self, _call_id: CallId, _function: telegram_api::Function) {
            self.queries.push("acceptCall".to_string());
        }

        fn send_confirm_call_query(&mut self, _call_id: CallId, _function: telegram_api::Function) {
            self.queries.push("confirmCall".to_string());
        }

        fn send_discard_call_query(&mut self, _call_id: CallId, _function: telegram_api::Function) {
            self.queries.push("discardCall".to_string());
        }

        fn send_received_call_query(
            &mut self,
            _call_id: CallId,
            _function: telegram_api::Function,
        ) {
            self.queries.push("receivedCall".to_string());
        }

        fn send_call_signaling_data_query(
            &mut self,
            _call_id: CallId,
            _function: telegram_api::Function,
        ) {
            self.queries.push("signaling".to_string());
        }
    }

    struct RecordedUnit(std::sync::Arc<std::sync::Mutex<Option<std::result::Result<(), Status>>>>);

    struct RecordingPromise(
        std::sync::Arc<std::sync::Mutex<Option<std::result::Result<(), Status>>>>,
    );

    impl tdutils::promise::PromiseInterface<()> for RecordingPromise {
        fn set_value(&mut self, _value: ()) {
            *self.0.lock().unwrap() = Some(Ok(()));
        }

        fn set_error(&mut self, error: Status) {
            *self.0.lock().unwrap() = Some(Err(error));
        }
    }

    fn record_promise() -> (Promise<()>, RecordedUnit) {
        let recorded = std::sync::Arc::new(std::sync::Mutex::new(None));
        (
            Promise::from_interface(RecordingPromise(recorded.clone())),
            RecordedUnit(recorded),
        )
    }

    #[test]
    fn create_call_sets_the_outgoing_pending_state_and_sends() {
        let mut env = TestEnv::new();
        // a good stored config (the built-in 2048-bit prime, g = 3 passes
        // the mod-4g gate) — a bad one now fails the load
        env.dh_config = Some(DhConfig {
            version: 1,
            prime: crate::dh_cache::BUILT_IN_GOOD_PRIME.to_vec(),
            g: 3,
        });
        let mut actor = CallActor::new(CallId::new(1));
        let call_id = actor.create_call(&mut env, UserId::new(5), CallProtocol::default(), false);
        assert_eq!(call_id, CallId::new(1));
        assert!(actor.is_outgoing);
        assert_eq!(actor.state, CallActorState::WaitRequestResult);
        assert_eq!(actor.call_state.type_, CallStateType::Pending);
        assert!(!actor.call_state_need_flush, "the flush ran in loop");
        assert!(env.queries.iter().any(|q| q == "requestCall"));
        assert_eq!(env.updates.len(), 1);
        match &env.updates[0] {
            td_api::Update::updateCall(update) => {
                let call = update.call.as_ref().unwrap();
                assert_eq!(call.user_id, 7);
                assert_eq!(call.id, 1);
            }
            _ => panic!("expected updateCall"),
        }
    }

    #[test]
    fn create_call_fails_on_a_bad_cached_dh_config() {
        // Every C++ actor validates the config through on_dh_config's
        // good-prime check (CallActor.cpp:653-666) — the cached store
        // read rides the NotModified fallback back into the same check
        // (cpp:683-701), so a bad stored prime fails the call instead of
        // riding the request leg
        let mut env = TestEnv::new();
        env.dh_config = Some(DhConfig {
            version: 1,
            prime: vec![0xff; 256],
            g: 3,
        });
        let mut actor = CallActor::new(CallId::new(1));
        actor.create_call(&mut env, UserId::new(5), CallProtocol::default(), false);
        assert!(
            !env.queries.iter().any(|q| q == "requestCall"),
            "the bad prime fails the load before the request leg"
        );
        assert!(
            !env.queries.iter().any(|q| q == "getDhConfig"),
            "the store serves the load — no fresh query"
        );
        match env.updates.last() {
            Some(td_api::Update::updateCall(update)) => {
                let call = update.call.as_ref().unwrap();
                match call.state.as_ref().unwrap() {
                    td_api::CallState::callStateError(state) => {
                        let error = state.error.as_ref().unwrap();
                        assert_eq!(error.message, "Bad prime mod 4g");
                    }
                    other => panic!("expected the error state: {other:?}"),
                }
            }
            other => panic!("expected updateCall: {other:?}"),
        }
        assert_eq!(actor.state, CallActorState::Discarded);
    }

    #[test]
    fn incoming_requested_call_asks_to_accept() {
        let mut env = TestEnv::new();
        let mut actor = CallActor::new(CallId::new(2));
        actor.update_call(
            &mut env,
            telegram_api::PhoneCall::phoneCallRequested(telegram_api::phoneCallRequested {
                flags: 0,
                video: false,
                id: 100,
                access_hash: 7,
                date: 0,
                admin_id: 8,
                participant_id: 5,
                g_a_hash: vec![9, 9, 9],
                protocol: Some(telegram_api::PhoneCallProtocol {
                    flags: 0,
                    udp_p2p: true,
                    udp_reflector: false,
                    min_layer: 65,
                    max_layer: 65,
                    library_versions: Vec::new(),
                }),
            }),
        );
        assert_eq!(actor.state, CallActorState::SendAcceptQuery);
        assert_eq!(actor.call_id, 100);
        assert!(actor.call_state.is_received);
        assert!(env.queries.iter().any(|q| q == "receivedCall"));
        assert_eq!(actor.call_admin_user_id, UserId::new(8));
    }

    #[test]
    fn accept_call_errors_outside_the_accept_state() {
        let mut env = TestEnv::new();
        let mut actor = CallActor::new(CallId::new(3));
        let (promise, recorded) = record_promise();
        actor.accept_call(&mut env, CallProtocol::default(), promise);
        assert!(
            recorded
                .0
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|r| r.is_err())
        );
    }

    #[test]
    fn discard_call_derives_the_reasons() {
        let mut env = TestEnv::new();
        let mut actor = CallActor::new(CallId::new(4));
        actor.state = CallActorState::WaitRequestResult;
        actor.is_outgoing = true;
        actor.call_state.type_ = CallStateType::Pending;
        let (promise, recorded) = record_promise();
        actor.discard_call(&mut env, false, "", 0, false, 0, promise);
        assert!(
            recorded
                .0
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|r| r.is_ok())
        );
        assert_eq!(
            actor.call_state.discard_reason.type_,
            CallDiscardReasonType::Missed
        );
        // the call id is still 0 → try_send_discard_query takes the
        // Missed fallback and the actor discards immediately
        assert_eq!(actor.state, CallActorState::Discarded);
        assert!(!env.queries.iter().any(|q| q == "discardCall"));
    }

    #[test]
    fn discard_call_with_a_bad_invite_link_errors() {
        let mut env = TestEnv::new();
        let mut actor = CallActor::new(CallId::new(5));
        actor.state = CallActorState::Ready;
        actor.call_state.type_ = CallStateType::Ready;
        actor.call_id = 42;
        let (promise, recorded) = record_promise();
        actor.discard_call(
            &mut env,
            false,
            "https://example.com/",
            0,
            false,
            0,
            promise,
        );
        assert!(
            recorded
                .0
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|r| r.is_err())
        );
        assert!(!env.queries.iter().any(|q| q == "discardCall"));
    }

    #[test]
    fn discarded_update_sets_the_final_state() {
        let mut env = TestEnv::new();
        let mut actor = CallActor::new(CallId::new(6));
        actor.state = CallActorState::WaitConfirmResult;
        actor.call_id = 42;
        actor.update_call(
            &mut env,
            telegram_api::PhoneCall::phoneCallDiscarded(telegram_api::phoneCallDiscarded {
                flags: 0,
                need_rating: true,
                need_debug: false,
                video: false,
                id: 42,
                reason: Some(
                    telegram_api::PhoneCallDiscardReason::phoneCallDiscardReasonHangup(
                        telegram_api::phoneCallDiscardReasonHangup,
                    ),
                ),
                duration: 3,
            }),
        );
        assert_eq!(actor.state, CallActorState::Discarded);
        assert_eq!(actor.call_state.type_, CallStateType::Discarded);
        assert!(actor.call_state.need_rating);
        assert_eq!(
            actor.call_state.discard_reason.type_,
            CallDiscardReasonType::HungUp
        );
    }

    #[test]
    fn recent_call_ids_round_trip_through_the_pmc() {
        let mut env = TestEnv::new();
        save_recent_call_full_ids(
            &mut env,
            &[CallFullId {
                call_id: 11,
                access_hash: 22,
            }],
        );
        assert_eq!(CallActor::get_recent_call_access_hash(&mut env, 11), 22);
        assert_eq!(CallActor::get_recent_call_access_hash(&mut env, 12), 0);

        let mut actor = CallActor::new(CallId::new(7));
        actor.call_id = 13;
        actor.call_access_hash = 33;
        actor.on_get_call_id(&mut env);
        assert_eq!(CallActor::get_recent_call_access_hash(&mut env, 13), 33);
        assert_eq!(load_recent_call_full_ids(&mut env).len(), 2);
    }

    #[test]
    fn emojis_fingerprint_has_four_entries() {
        let fingerprint = CallActor::get_emojis_fingerprint(b"key", b"g_a".to_vec());
        assert_eq!(fingerprint.len(), 4);
        // sha256(key + g_a) drives the table exactly like the C++
        assert_eq!(fingerprint, get_emoji_fingerprints(&sha256(b"keyg_a")));
    }

    #[test]
    fn signaling_data_builds_the_query_only_when_ready() {
        let mut actor = CallActor::new(CallId::new(1));
        // the Empty-state gate (cpp:273-275)
        assert!(matches!(
            actor.send_call_signaling_data(b"sig".to_vec()),
            Err(ref status) if status.message() == "Call is not active"
        ));

        // the Ready-state build (cpp:276-279): the peer + the payload
        // ride the phone.sendSignalingData body
        actor.call_id = 100;
        actor.call_access_hash = 7;
        actor.is_call_id_inited = true;
        actor.call_state.type_ = CallStateType::Ready;
        let function = actor
            .send_call_signaling_data(b"sig".to_vec())
            .expect("the ready state builds the query");
        match function {
            telegram_api::Function::phone_sendSignalingData(query) => {
                let peer = query.peer.expect("the input phone call");
                assert_eq!((peer.id, peer.access_hash), (100, 7));
                assert_eq!(query.data, b"sig".to_vec());
            }
            other => panic!("the signaling body: {other:?}"),
        }
    }
}
