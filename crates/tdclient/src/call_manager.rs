//! `CallManager` (td/telegram/CallManager.{h,cpp}) — the M.32 Wave-3 call
//! manager. The C++ actor model collapses to a plain struct (the M.13
//! convention): `ActorOwn<CallActor>` becomes a map value and the
//! `SafePromise` hops become direct calls. The net-drain 44 slice lands
//! the `CallNetKind` plane over the manager-held net handle:
//! `phone.setCallRating` (SetCallRatingQuery, cpp:31-62) and
//! `phone.saveCallDebug` (SaveCallDebugQuery, cpp:64-94) ride the
//! dispatcher — `sendCallRating` routes here through `rate_call`
//! (Requests.cpp:5132-5138) and the answers drain through
//! `Td::on_call_net_answer`; `phone.saveCallLog` is `[blocked:
//! net/Files]` — the C++ send resolves the uploaded InputFile through
//! the FileManager upload chain (CallManager.cpp:368-386), so the
//! request parks at the upload seam with the answer intake landed. The
//! net-drain 54 slice extends the same plane with the eight actor
//! families (CallActor.cpp): the actor builds the TL at the send site,
//! the `CallEnv` override dispatches, and the answers route back into
//! the actors through the `on_*_net_answer` intakes below.

use crate::call_actor::{CallActor, CallActorEnv};
use crate::call_id::{CallId, InputCallId};
use crate::call_protocol::CallProtocol;
use crate::dh_cache::{DhCacheEnv, DhConfig};
use crate::dialog_id::MessageFullId;
use crate::td_api;
use crate::user_manager::UserId;
use tdmtproto::telegram_api;
use tdutils::promise::Promise;
use tdutils::status::Status;

/// The `G()`/`td_` surface of the manager.
pub trait CallManagerEnv: CallActorEnv + DhCacheEnv {
    /// `G()->close_status()` — the close gate of the C++ request tails.
    fn close_status(&self) -> Status;

    /// `MessagesManager::get_input_phone_call_to_promise` — the message
    /// variant of [`CallManager::fetch_input_phone_call`]; `None` until
    /// M.10 lands (the C++ errors "Call not found" on an empty answer).
    fn get_input_phone_call_from_message(
        &mut self,
        _message_full_id: MessageFullId,
    ) -> Option<telegram_api::InputPhoneCall> {
        None
    }
}

/// `CallManager::CallInfo` (CallManager.h:60-63).
#[derive(Debug, Default)]
struct CallInfo {
    call_id: CallId,
    updates: Vec<telegram_api::PhoneCall>,
}

/// The collapsed `CallManager`.
#[derive(Default)]
pub struct CallManager {
    close_flag: bool,
    /// `next_call_id_` (CallManager.h:65).
    next_call_id: i32,
    /// `call_info_` (CallManager.h:64) — the server call id → info map.
    call_info: std::collections::BTreeMap<i64, CallInfo>,
    /// `id_to_actor_` (CallManager.h:66) — the collapsed `ActorOwn` map.
    id_to_actor: std::collections::BTreeMap<CallId, CallActor>,
    /// The manager-held dispatch surface (`UserManager::net` shape) —
    /// `None` keeps the net-inert posture (the pre-init windows and the
    /// net-inert tests).
    pub(crate) net: Option<crate::td::CallNetHandle>,
    /// The parked query promises (the [`PendingCallQueries`] lots).
    pub(crate) pending: PendingCallQueries,
}

/// The CallManager in-flight query kinds — the C++ ResultHandler classes
/// of the net-drain 44 families (CallManager.cpp:31-118). The handler
/// state the answers read (the call the answer routes into) rides the
/// kinds — [`InputCallId`] is `Copy`.
#[derive(Clone, Copy)]
pub(crate) enum CallNetKind {
    /// `SetCallRatingQuery` (cpp:31-62) — the ok answer routes
    /// [`CallManager::on_set_call_rating`] + the Updates engine intake
    /// with the request promise (cpp:52-60).
    SetCallRating { call_id: InputCallId },
    /// `SaveCallDebugQuery` (cpp:64-94) — the ok answer routes
    /// [`CallManager::on_save_debug_information`] with the Bool result
    /// and settles the request promise (cpp:87-93).
    SaveCallDebug { call_id: InputCallId },
    /// `SaveCallLogQuery` (cpp:96-118) — the send rides the M.12 upload
    /// chain; the answer routes [`CallManager::on_save_log`]
    /// (cpp:115-127). Un-gates with the M.12 Files plane (the send
    /// dispatch arrives with the upload chain).
    #[allow(dead_code)]
    SaveCallLog { call_id: InputCallId },
    /// `do_load_dh_config`'s `messages.getDhConfig` (CallActor.cpp:668-703)
    /// — the answer parses in the Td router (the DhConfig build + the
    /// DRBG seed + the stored-config fallback, cpp:683-701) and routes
    /// [`CallManager::on_dh_config_net_answer`].
    GetDhConfig { call_id: CallId },
    /// `start_up`'s `phone.getCallConfig` (CallActor.cpp:884-900) — the
    /// dataJSON answer routes
    /// [`CallManager::on_call_config_net_answer`].
    GetCallConfig { call_id: CallId },
    /// `try_send_request_query`'s `phone.requestCall` (CallActor.cpp:721-758)
    /// — the `phone.phoneCall` answer rides
    /// [`CallManager::on_phone_call_net_answer`].
    RequestCall { call_id: CallId },
    /// `try_send_accept_query`'s `phone.acceptCall` (CallActor.cpp:760-789)
    /// — the answer rides [`CallManager::on_phone_call_net_answer`].
    AcceptCall { call_id: CallId },
    /// `try_send_confirm_query`'s `phone.confirmCall` (CallActor.cpp:791-812)
    /// — the answer rides [`CallManager::on_phone_call_net_answer`].
    ConfirmCall { call_id: CallId },
    /// `try_send_discard_query`'s `phone.discardCall` (CallActor.cpp:814-841)
    /// — the ok answer rides the Updates engine (the Td router um_sends
    /// it with an empty promise, cpp:837-841), the fetch error routes
    /// [`CallManager::on_discard_call_net_answer`].
    DiscardCall { call_id: CallId },
    /// `send_received_query`'s `phone.receivedCall` (CallActor.cpp:705-719)
    /// — the ok answer is unused, the fetch error routes
    /// [`CallManager::on_received_call_net_answer`].
    ReceivedCall { call_id: CallId },
    /// `send_call_signaling_data`'s `phone.sendSignalingData`
    /// (CallActor.cpp:271-292) — the promise rides the FIFO lot; the
    /// answer settles it in the Td router with no actor tail
    /// (cpp:280-289), so the routing id never reads back (the
    /// `SaveCallLog` allow precedent).
    #[allow(dead_code)]
    SendSignalingData { call_id: CallId },
}

/// The parked query promises — the ResultHandler members the `Copy`
/// [`CallNetKind`]s can't carry. FIFO-paired per family (§5; the C++
/// carries the promise on the query itself).
#[derive(Default)]
pub(crate) struct PendingCallQueries {
    /// `SetCallRatingQuery::promise_` (cpp:33).
    pub(crate) set_call_rating: Vec<Promise<()>>,
    /// `SaveCallDebugQuery::promise_` (cpp:66).
    pub(crate) save_call_debug: Vec<Promise<()>>,
    /// `SaveCallLogQuery::promise_` (cpp:99) — the send parks over the
    /// M.12 upload chain; the answer intake pops the lot.
    pub(crate) save_call_log: Vec<Promise<()>>,
    /// `send_call_signaling_data`'s request promise (CallActor.cpp:280-289
    /// — the C++ carries it on the query); the Td router pops the lot.
    pub(crate) send_signaling_data: Vec<Promise<()>>,
}

impl CallManager {
    /// `CallManager()` (CallManager.cpp:131-132).
    pub fn new() -> CallManager {
        CallManager {
            close_flag: false,
            next_call_id: 1,
            call_info: std::collections::BTreeMap::new(),
            id_to_actor: std::collections::BTreeMap::new(),
            net: None,
            pending: PendingCallQueries::default(),
        }
    }

    /// The `td_->create_handler<…>()->send(...)` collapse — the wire
    /// query construction lives at the manager send sites; `None` keeps
    /// the net-inert posture (the tests drive the answer halves).
    fn send_call_net_query(
        &self,
        kind: CallNetKind,
        function: tdmtproto::telegram_api::Function,
        what: &str,
    ) {
        match &self.net {
            Some(net) => net.dispatch(kind, function),
            None => tdutils::log_info!("Send {what} — [net-inert]"),
        }
    }

    /// `update_call` (CallManager.cpp:138-177).
    pub fn update_call(
        &mut self,
        env: &mut impl CallManagerEnv,
        call: telegram_api::updatePhoneCall,
    ) {
        let phone_call = call.phone_call.expect("updatePhoneCall carries a call");
        let call_id = match &phone_call {
            telegram_api::PhoneCall::phoneCallEmpty(call) => call.id,
            telegram_api::PhoneCall::phoneCallWaiting(call) => call.id,
            telegram_api::PhoneCall::phoneCallRequested(call) => call.id,
            telegram_api::PhoneCall::phoneCallAccepted(call) => call.id,
            telegram_api::PhoneCall::phoneCall(call) => call.id,
            telegram_api::PhoneCall::phoneCallDiscarded(call) => call.id,
        };
        tdutils::log_info!("Receive UpdateCall for {call_id}");

        let is_requested = matches!(&phone_call, telegram_api::PhoneCall::phoneCallRequested(_));
        if is_requested
            && !self
                .call_info
                .get(&call_id)
                .is_some_and(|info| info.call_id.is_valid())
        {
            let new_call_id = self.create_call_actor(env);
            self.call_info.entry(call_id).or_default().call_id = new_call_id;
        }
        let info = self.call_info.entry(call_id).or_default();

        if !info.call_id.is_valid() {
            tdutils::log_info!("Call identifier is not valid for {call_id}, postpone update");
            info.updates.push(phone_call);
            return;
        }

        let info_call_id = info.call_id;
        let should_close = match self.id_to_actor.get_mut(&info_call_id) {
            Some(actor) => {
                actor.update_call(env, phone_call);
                actor.should_stop()
            }
            None => {
                tdutils::log_info!("Drop update for {call_id}");
                false
            }
        };
        if should_close {
            self.id_to_actor.remove(&info_call_id);
        }
    }

    /// `update_call_signaling_data` (CallManager.cpp:179-192).
    pub fn update_call_signaling_data(
        &mut self,
        env: &mut impl CallManagerEnv,
        call_id: i64,
        data: Vec<u8>,
    ) {
        let Some(info) = self.call_info.get(&call_id) else {
            return;
        };
        if !info.call_id.is_valid() {
            return;
        }
        let info_call_id = info.call_id;
        if let Some(actor) = self.id_to_actor.get_mut(&info_call_id) {
            actor.update_call_signaling_data(env, data);
        }
    }

    /// `create_call` (CallManager.cpp:194-202).
    pub fn create_call(
        &mut self,
        env: &mut impl CallManagerEnv,
        user_id: UserId,
        protocol: CallProtocol,
        is_video: bool,
        mut promise: Promise<td_api::CallId>,
    ) {
        // `TRY_STATUS_PROMISE(promise, td_->user_manager_->get_input_user(user_id))`
        if let Err(status) = env.get_input_user(user_id) {
            return promise.set_error(status);
        }
        tdutils::log_info!("Create call with {user_id:?}");
        let call_id = self.create_call_actor(env);
        let actor = self
            .id_to_actor
            .get_mut(&call_id)
            .expect("the call actor just created");
        let local_call_id = actor.create_call(env, user_id, protocol, is_video);
        promise.set_value(local_call_id.get_call_id_object());
    }

    /// `accept_call` (CallManager.cpp:204-211).
    pub fn accept_call(
        &mut self,
        env: &mut impl CallManagerEnv,
        call_id: CallId,
        protocol: CallProtocol,
        mut promise: Promise<()>,
    ) {
        let Some(actor) = self.id_to_actor.get_mut(&call_id) else {
            return promise.set_error(Status::error(400, "Call not found"));
        };
        actor.accept_call(env, protocol, promise);
    }

    /// `send_call_signaling_data` (CallManager.cpp:213-220) — the actor
    /// runs the state gate + the TL build (CallActor.cpp:272-289), then
    /// the promise parks in the FIFO family lot and the dispatch rides
    /// the env seam (the `do_send_call_debug_information` ordering,
    /// cpp:365). The C++ carries the promise on the query itself
    /// (cpp:280-289).
    pub fn send_call_signaling_data(
        &mut self,
        env: &mut impl CallManagerEnv,
        call_id: CallId,
        data: Vec<u8>,
        mut promise: Promise<()>,
    ) {
        let Some(actor) = self.id_to_actor.get_mut(&call_id) else {
            return promise.set_error(Status::error(400, "Call not found"));
        };
        let function = match actor.send_call_signaling_data(data) {
            Ok(function) => function,
            Err(status) => return promise.set_error(status),
        };
        self.pending.send_signaling_data.push(promise);
        env.send_call_signaling_data_query(call_id, function);
    }

    /// `discard_call` (CallManager.cpp:222-231) — the C++ signature shape
    /// (the td_api discardCall carries the six call parameters).
    #[allow(clippy::too_many_arguments)]
    pub fn discard_call(
        &mut self,
        env: &mut impl CallManagerEnv,
        call_id: CallId,
        is_disconnected: bool,
        invite_link: &str,
        duration: i32,
        is_video: bool,
        connection_id: i64,
        mut promise: Promise<()>,
    ) {
        let Some(actor) = self.id_to_actor.get_mut(&call_id) else {
            return promise.set_error(Status::error(400, "Call not found"));
        };
        actor.discard_call(
            env,
            is_disconnected,
            invite_link,
            duration,
            is_video,
            connection_id,
            promise,
        );
    }

    /// `fetch_input_phone_call` (CallManager.cpp:233-254).
    pub fn fetch_input_phone_call(
        &mut self,
        env: &mut impl CallManagerEnv,
        call_id: InputCallId,
        mut promise: Promise<telegram_api::InputPhoneCall>,
    ) {
        match call_id {
            InputCallId::Call(call_id) => {
                let Some(actor) = self.id_to_actor.get(&call_id) else {
                    return promise.set_error(Status::error(400, "Call not found"));
                };
                actor.get_input_phone_call_to_promise(promise);
            }
            InputCallId::Message(message_full_id) => {
                match env.get_input_phone_call_from_message(message_full_id) {
                    Some(input_phone_call) => promise.set_value(input_phone_call),
                    None => promise.set_error(Status::error(400, "Call not found")),
                }
            }
        }
    }

    /// `send_call_debug_information` (CallManager.cpp:337-350) — the
    /// resolved inputPhoneCall feeds the `SaveCallDebugQuery` send.
    pub fn send_call_debug_information(
        &mut self,
        env: &mut impl CallManagerEnv,
        call_id: InputCallId,
        data: String,
        mut promise: Promise<()>,
    ) {
        let status = env.close_status();
        if status.is_error() {
            return promise.set_error(status);
        }
        match call_id {
            InputCallId::Call(call_id) => {
                let Some(actor) = self.id_to_actor.get(&call_id) else {
                    return promise.set_error(Status::error(400, "Call not found"));
                };
                let (inner, recorded) = record_phone_call_promise();
                actor.get_input_phone_call_to_promise(inner);
                let Some(input_phone_call) = recorded.lock().unwrap().take() else {
                    return promise.set_error(Status::error(400, "Call not found"));
                };
                self.do_send_call_debug_information(
                    env,
                    InputCallId::Call(call_id),
                    input_phone_call,
                    data,
                    promise,
                );
            }
            InputCallId::Message(message_full_id) => {
                let Some(input_phone_call) = env.get_input_phone_call_from_message(message_full_id)
                else {
                    return promise.set_error(Status::error(400, "Call not found"));
                };
                self.do_send_call_debug_information(
                    env,
                    InputCallId::Message(message_full_id),
                    input_phone_call,
                    data,
                    promise,
                );
            }
        };
    }

    /// `do_send_call_debug_information` (CallManager.cpp:352-357) — the
    /// `phone.saveCallDebug` send; the promise rides the family queue.
    fn do_send_call_debug_information(
        &mut self,
        env: &mut impl CallManagerEnv,
        call_id: InputCallId,
        input_phone_call: telegram_api::InputPhoneCall,
        data: String,
        promise: Promise<()>,
    ) {
        let status = env.close_status();
        if status.is_error() {
            let mut promise = promise;
            return promise.set_error(status);
        }
        self.pending.save_call_debug.push(promise);
        self.send_call_net_query(
            CallNetKind::SaveCallDebug { call_id },
            tdmtproto::telegram_api::Function::phone_saveCallDebug(
                telegram_api::phone_saveCallDebug {
                    peer: Some(input_phone_call),
                    debug: Some(telegram_api::DataJSON { data }),
                },
            ),
            "phone.saveCallDebug",
        );
    }

    /// `send_call_log` (CallManager.cpp:368-386) — the close gate and
    /// the InputCallId resolution bands are real; the file intake
    /// (`FileManager::get_input_file_id(FileType::CallLog)` + the
    /// encrypted-file band) and the upload chain ride the M.12 Files
    /// surface — the request parks at the upload seam and the promise
    /// waits in the family queue for the landed answer intake.
    pub fn send_call_log(
        &mut self,
        env: &mut impl CallManagerEnv,
        input_call: &Option<td_api::InputCall>,
        log_file: Option<td_api::InputFile>,
        mut promise: Promise<()>,
    ) {
        let status = env.close_status();
        if status.is_error() {
            return promise.set_error(status);
        }
        if let Err(status) = InputCallId::get_input_call_id(input_call) {
            return promise.set_error(status);
        }
        tdutils::log_info!(
            "Send phone.saveCallLog(has_file = {}) — [blocked: net/Files] over the M.12 upload chain",
            log_file.is_some()
        );
        self.pending.save_call_log.push(promise);
    }

    /// `rate_call` (CallManager.cpp:256-273) — the resolved inputPhoneCall
    /// feeds the `SetCallRatingQuery` send.
    pub fn rate_call(
        &mut self,
        env: &mut impl CallManagerEnv,
        input_call: &Option<td_api::InputCall>,
        rating: i32,
        comment: String,
        problems: Vec<td_api::CallProblem>,
        mut promise: Promise<()>,
    ) {
        let status = env.close_status();
        if status.is_error() {
            return promise.set_error(status);
        }
        if !(1..=5).contains(&rating) {
            return promise.set_error(Status::error(400, "Invalid rating specified"));
        }
        let Ok(call_id) = InputCallId::get_input_call_id(input_call) else {
            let status = InputCallId::get_input_call_id(input_call).unwrap_err();
            return promise.set_error(status);
        };
        let (inner, recorded) = record_phone_call_promise();
        self.fetch_input_phone_call(env, call_id, inner);
        let Some(input_phone_call) = recorded.lock().unwrap().take() else {
            return promise.set_error(Status::error(400, "Call not found"));
        };
        self.do_rate_call(
            env,
            call_id,
            input_phone_call,
            rating,
            comment,
            &problems,
            promise,
        );
    }

    /// `do_rate_call` (CallManager.cpp:275-326) — the tag-building and the
    /// `phone.setCallRating` send; the promise rides the family queue.
    #[allow(clippy::too_many_arguments)]
    fn do_rate_call(
        &mut self,
        env: &mut impl CallManagerEnv,
        call_id: InputCallId,
        input_phone_call: telegram_api::InputPhoneCall,
        rating: i32,
        mut comment: String,
        problems: &[td_api::CallProblem],
        promise: Promise<()>,
    ) {
        let status = env.close_status();
        if status.is_error() {
            let mut promise = promise;
            return promise.set_error(status);
        }
        if rating == 5 {
            comment.clear();
        }

        let mut tags = std::collections::BTreeSet::new();
        for problem in problems {
            let tag = match problem {
                td_api::CallProblem::callProblemEcho(_) => "echo",
                td_api::CallProblem::callProblemNoise(_) => "noise",
                td_api::CallProblem::callProblemInterruptions(_) => "interruptions",
                td_api::CallProblem::callProblemDistortedSpeech(_) => "distorted_speech",
                td_api::CallProblem::callProblemSilentLocal(_) => "silent_local",
                td_api::CallProblem::callProblemSilentRemote(_) => "silent_remote",
                td_api::CallProblem::callProblemDropped(_) => "dropped",
                td_api::CallProblem::callProblemDistortedVideo(_) => "distorted_video",
                td_api::CallProblem::callProblemPixelatedVideo(_) => "pixelated_video",
            };
            if tags.insert(tag) {
                if !comment.is_empty() {
                    comment.push(' ');
                }
                comment.push('#');
                comment.push_str(tag);
            }
        }
        self.pending.set_call_rating.push(promise);
        self.send_call_net_query(
            CallNetKind::SetCallRating { call_id },
            tdmtproto::telegram_api::Function::phone_setCallRating(
                telegram_api::phone_setCallRating {
                    flags: 0,
                    user_initiative: false,
                    peer: Some(input_phone_call),
                    rating,
                    comment,
                },
            ),
            "phone.setCallRating",
        );
    }

    /// `on_set_call_rating` (CallManager.cpp:328-333) — the
    /// `SetCallRatingQuery` ok-answer intake.
    pub fn on_set_call_rating(&mut self, env: &mut impl CallManagerEnv, call_id: InputCallId) {
        if let InputCallId::Call(call_id) = call_id
            && let Some(actor) = self.id_to_actor.get_mut(&call_id)
        {
            actor.on_set_call_rating(env);
        }
    }

    /// `on_save_debug_information` (CallManager.cpp:359-366).
    pub fn on_save_debug_information(
        &mut self,
        env: &mut impl CallManagerEnv,
        call_id: InputCallId,
        result: bool,
    ) {
        if let InputCallId::Call(call_id) = call_id
            && let Some(actor) = self.id_to_actor.get_mut(&call_id)
        {
            actor.on_save_debug_information(env, result);
        }
    }

    /// `on_save_log` (CallManager.cpp:460-478) — the ok leg's actor tail;
    /// the FileUploadId partial-location cleanup and the promise settle
    /// ride the Td answer router (cpp:462/:471/:477).
    pub fn on_save_log(&mut self, env: &mut impl CallManagerEnv, call_id: InputCallId) {
        if let InputCallId::Call(call_id) = call_id
            && let Some(actor) = self.id_to_actor.get_mut(&call_id)
        {
            actor.on_save_log(env);
        }
    }

    /// The `GetDhConfigQuery` answer intake — the `on_dh_config` tail
    /// (CallActor.cpp:653-666). The DhConfig build + the DRBG seed + the
    /// stored-config fallback live in the Td router (the C++ fetch
    /// lambda, cpp:683-701); the actor-gone case just skips the tail
    /// (the C++ handler settles independent of the actor).
    pub(crate) fn on_dh_config_net_answer(
        &mut self,
        env: &mut impl CallManagerEnv,
        call_id: CallId,
        result: std::result::Result<DhConfig, Status>,
    ) {
        let Some(actor) = self.id_to_actor.get_mut(&call_id) else {
            return;
        };
        actor.on_dh_config(env, result);
        actor.loop_(env);
        if actor.should_stop() {
            self.id_to_actor.remove(&call_id);
        }
    }

    /// The `GetCallConfigQuery` answer intake — the
    /// `on_get_call_config_result` tail (CallActor.cpp:893-900).
    pub(crate) fn on_call_config_net_answer(
        &mut self,
        env: &mut impl CallManagerEnv,
        call_id: CallId,
        result: std::result::Result<String, Status>,
    ) {
        let Some(actor) = self.id_to_actor.get_mut(&call_id) else {
            return;
        };
        actor.on_get_call_config_result(env, result);
        actor.loop_(env);
        if actor.should_stop() {
            self.id_to_actor.remove(&call_id);
        }
    }

    /// The `phone.phoneCall` answer intake of the request/accept/confirm
    /// families — the `update_call_inner` tail (CallActor.cpp:414-418)
    /// with the post-answer loop sweep (the C++ `on_result` yield
    /// always runs the loop after the answer event, cpp:941-944) and the
    /// manager `should_stop` sweep (the `update_call` shape).
    pub(crate) fn on_phone_call_net_answer(
        &mut self,
        env: &mut impl CallManagerEnv,
        call_id: CallId,
        result: std::result::Result<telegram_api::phone_PhoneCall, Status>,
    ) {
        let Some(actor) = self.id_to_actor.get_mut(&call_id) else {
            return;
        };
        match result {
            Ok(call) => actor.update_call_inner(env, call),
            Err(status) => actor.on_error(env, status),
        }
        actor.loop_(env);
        let server_call_id = actor.call_id;
        if actor.should_stop() {
            self.id_to_actor.remove(&call_id);
            return;
        }
        // The collapsed create-time `main_promise` hop (CallManager.cpp:486-491
        // → `set_call_id` cpp:499-514): the actor learned the server call
        // id from the answer (the C++ `on_get_call_id` fires the
        // promise); bind it and replay any postponed updates.
        if server_call_id != 0 {
            self.set_call_id(env, call_id, std::result::Result::Ok(server_call_id));
        }
    }

    /// The `ReceivedCallQuery` answer intake — the
    /// `on_received_query_result` tail (CallActor.cpp:714-719): the ok
    /// answer is unused, the fetch error routes into `on_error`.
    pub(crate) fn on_received_call_net_answer(
        &mut self,
        env: &mut impl CallManagerEnv,
        call_id: CallId,
        result: std::result::Result<(), Status>,
    ) {
        let Some(actor) = self.id_to_actor.get_mut(&call_id) else {
            return;
        };
        if let Err(status) = result {
            actor.on_error(env, status);
        }
        actor.loop_(env);
        if actor.should_stop() {
            self.id_to_actor.remove(&call_id);
        }
    }

    /// The `DiscardCallQuery` answer intake — the
    /// `on_discard_query_result` error tail (CallActor.cpp:837-841); the
    /// ok answer never reaches the actor (the Td router um_sends the
    /// Updates to the engine with an empty promise before this runs).
    pub(crate) fn on_discard_call_net_answer(
        &mut self,
        env: &mut impl CallManagerEnv,
        call_id: CallId,
        result: std::result::Result<(), Status>,
    ) {
        let Some(actor) = self.id_to_actor.get_mut(&call_id) else {
            return;
        };
        if let Err(status) = result {
            actor.on_error(env, status);
        }
        actor.loop_(env);
        if actor.should_stop() {
            self.id_to_actor.remove(&call_id);
        }
    }

    /// `create_call_actor` (CallManager.cpp:482-496) — the actor's
    /// `start_up` (the getCallConfig query) fires at creation, the C++
    /// actor-start moment.
    fn create_call_actor(&mut self, env: &mut impl CallManagerEnv) -> CallId {
        if self.next_call_id == i32::MAX {
            self.next_call_id = 1;
        }
        let id = CallId::new(self.next_call_id);
        self.next_call_id += 1;
        debug_assert!(id.is_valid());
        let actor = CallActor::new(id);
        self.id_to_actor.insert(id, actor);
        self.id_to_actor
            .get_mut(&id)
            .expect("the call actor just created")
            .start_up(env);
        id
    }

    /// `set_call_id` (CallManager.cpp:499-515) — the server-call-id
    /// binding + the postponed-update replay.
    pub fn set_call_id(
        &mut self,
        env: &mut impl CallManagerEnv,
        call_id: CallId,
        result: std::result::Result<i64, Status>,
    ) {
        let Ok(server_call_id) = result else {
            return;
        };
        let info = self.call_info.entry(server_call_id).or_default();
        debug_assert!(!info.call_id.is_valid() || info.call_id == call_id);
        info.call_id = call_id;
        let updates = std::mem::take(&mut info.updates);
        if let Some(actor) = self.id_to_actor.get_mut(&call_id) {
            for update in updates {
                actor.update_call(env, update);
            }
        }
    }

    /// `get_call_actor` (CallManager.cpp:517-523).
    pub fn get_call_actor(&self, call_id: CallId) -> Option<&CallActor> {
        self.id_to_actor.get(&call_id)
    }

    /// `hangup` (CallManager.cpp:525-534) — the collapsed close: drop the
    /// actors and remember the close flag (the C++ stops once empty).
    pub fn hangup(&mut self) {
        self.close_flag = true;
        self.id_to_actor.clear();
    }
}

/// A promise recorder for the inputPhoneCall hop (the collapsed
/// `PromiseCreator::lambda` chain).
fn record_phone_call_promise() -> (
    Promise<telegram_api::InputPhoneCall>,
    std::sync::Arc<std::sync::Mutex<Option<telegram_api::InputPhoneCall>>>,
) {
    struct RecordingPhoneCall(
        std::sync::Arc<std::sync::Mutex<Option<telegram_api::InputPhoneCall>>>,
    );
    impl tdutils::promise::PromiseInterface<telegram_api::InputPhoneCall> for RecordingPhoneCall {
        fn set_value(&mut self, value: telegram_api::InputPhoneCall) {
            *self.0.lock().unwrap() = Some(value);
        }

        fn set_error(&mut self, _error: Status) {}
    }
    let recorded = std::sync::Arc::new(std::sync::Mutex::new(None));
    (
        Promise::from_interface(RecordingPhoneCall(recorded.clone())),
        recorded,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat_manager::DialogId;
    use crate::dh_cache::DhConfig;
    use std::collections::BTreeMap;

    struct TestEnv {
        pmc: BTreeMap<Vec<u8>, Vec<u8>>,
        dh_config: Option<DhConfig>,
        queries: Vec<String>,
    }

    impl TestEnv {
        fn new() -> TestEnv {
            TestEnv {
                pmc: BTreeMap::new(),
                dh_config: None,
                queries: Vec::new(),
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
            7
        }

        fn add_call_notification(&mut self, _dialog_id: DialogId, _call_id: CallId) {}

        fn remove_call_notification(&mut self, _dialog_id: DialogId, _call_id: CallId) {}

        fn send_update(&mut self, _update: td_api::Update) {}

        fn send_request_call_query(&mut self, _call_id: CallId, _function: telegram_api::Function) {
            self.queries.push("requestCall".to_string());
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
    }

    impl CallManagerEnv for TestEnv {
        fn close_status(&self) -> Status {
            Status::ok()
        }
    }

    struct Recorded<T>(std::sync::Arc<std::sync::Mutex<Option<std::result::Result<T, Status>>>>);

    struct Recording<T: Send + 'static>(
        std::sync::Arc<std::sync::Mutex<Option<std::result::Result<T, Status>>>>,
    );

    impl<T: Send + 'static> tdutils::promise::PromiseInterface<T> for Recording<T> {
        fn set_value(&mut self, value: T) {
            *self.0.lock().unwrap() = Some(Ok(value));
        }

        fn set_error(&mut self, error: Status) {
            *self.0.lock().unwrap() = Some(Err(error));
        }
    }

    fn record<T: Send + 'static>() -> (Promise<T>, Recorded<T>) {
        let recorded = std::sync::Arc::new(std::sync::Mutex::new(None));
        (
            Promise::from_interface(Recording(recorded.clone())),
            Recorded(recorded),
        )
    }

    #[test]
    fn create_call_allocates_and_dispatches() {
        let mut env = TestEnv::new();
        // a good stored config — a bad one fails the load (the C++
        // on_dh_config gate, CallActor.cpp:653-666)
        env.dh_config = Some(DhConfig {
            version: 1,
            prime: crate::dh_cache::BUILT_IN_GOOD_PRIME.to_vec(),
            g: 3,
        });
        let mut manager = CallManager::new();
        let (promise, recorded) = record::<td_api::CallId>();
        manager.create_call(
            &mut env,
            UserId::new(5),
            CallProtocol::default(),
            false,
            promise,
        );
        let answer = recorded.0.lock().unwrap().take().unwrap().unwrap();
        assert_eq!(answer.id, 1);
        assert!(env.queries.iter().any(|q| q == "requestCall"));
    }

    #[test]
    fn requested_update_creates_an_actor_and_routes() {
        let mut env = TestEnv::new();
        let mut manager = CallManager::new();
        manager.update_call(
            &mut env,
            telegram_api::updatePhoneCall {
                phone_call: Some(telegram_api::PhoneCall::phoneCallRequested(
                    telegram_api::phoneCallRequested {
                        flags: 0,
                        video: false,
                        id: 100,
                        access_hash: 7,
                        date: 0,
                        admin_id: 8,
                        participant_id: 5,
                        g_a_hash: vec![1],
                        protocol: Some(telegram_api::PhoneCallProtocol {
                            flags: 0,
                            udp_p2p: true,
                            udp_reflector: false,
                            min_layer: 65,
                            max_layer: 65,
                            library_versions: Vec::new(),
                        }),
                    },
                )),
            },
        );
        assert_eq!(manager.id_to_actor.len(), 1);
        assert!(env.queries.iter().any(|q| q == "receivedCall"));
        let info = manager.call_info.get(&100).unwrap();
        assert!(info.call_id.is_valid());
    }

    #[test]
    fn update_for_an_unknown_call_postpones_until_the_id_binds() {
        let mut env = TestEnv::new();
        let mut manager = CallManager::new();
        manager.update_call(
            &mut env,
            telegram_api::updatePhoneCall {
                phone_call: Some(telegram_api::PhoneCall::phoneCallWaiting(
                    telegram_api::phoneCallWaiting {
                        flags: 0,
                        video: false,
                        id: 101,
                        access_hash: 3,
                        date: 0,
                        admin_id: 0,
                        participant_id: 0,
                        protocol: None,
                        receive_date: 0,
                    },
                )),
            },
        );
        assert!(manager.id_to_actor.is_empty());
        let info = manager.call_info.get(&101).unwrap();
        assert_eq!(info.updates.len(), 1, "the update is postponed");
    }

    #[test]
    fn accept_and_discard_answer_call_not_found() {
        let mut env = TestEnv::new();
        let mut manager = CallManager::new();
        let (promise, recorded) = record::<()>();
        manager.accept_call(&mut env, CallId::new(9), CallProtocol::default(), promise);
        assert!(recorded.0.lock().unwrap().is_some());
        let (promise, recorded) = record::<()>();
        manager.discard_call(&mut env, CallId::new(9), false, "", 0, false, 0, promise);
        assert!(recorded.0.lock().unwrap().is_some());
    }

    #[test]
    fn debug_information_errors_on_the_unknown_call_band() {
        let mut env = TestEnv::new();
        let mut manager = CallManager::new();
        let (promise, recorded) = record::<()>();
        manager.send_call_debug_information(
            &mut env,
            InputCallId::Call(CallId::new(9)),
            "data".to_string(),
            promise,
        );
        // the unknown call errors before any query
        assert!(matches!(
            recorded.0.lock().unwrap().take().unwrap(),
            Result::Err(ref status) if status.message() == "Call not found"
        ));
        assert!(manager.pending.save_call_debug.is_empty());
    }

    #[test]
    fn do_rate_call_parks_in_the_family_queue_when_net_inert() {
        let mut env = TestEnv::new();
        let mut manager = CallManager::new();
        let (promise, recorded) = record::<()>();
        manager.do_rate_call(
            &mut env,
            InputCallId::Call(CallId::new(1)),
            telegram_api::InputPhoneCall {
                id: 100,
                access_hash: 7,
            },
            4,
            "great".to_string(),
            &[
                td_api::CallProblem::callProblemEcho(td_api::callProblemEcho),
                td_api::CallProblem::callProblemDropped(td_api::callProblemDropped),
            ],
            promise,
        );
        // net-inert: the promise waits in the family queue, nothing settles
        assert!(recorded.0.lock().unwrap().is_none());
        assert_eq!(manager.pending.set_call_rating.len(), 1);
        assert!(manager.pending.save_call_debug.is_empty());
    }

    #[test]
    fn send_call_log_validates_the_input_call_and_parks() {
        let mut env = TestEnv::new();
        let mut manager = CallManager::new();

        // a missing input call errors on the resolution band
        let (promise, recorded) = record::<()>();
        manager.send_call_log(&mut env, &None, None, promise);
        assert!(matches!(
            recorded.0.lock().unwrap().take().unwrap(),
            Result::Err(ref status) if status.message() == "Input call must be non-empty"
        ));

        // a call id rides to the upload seam and parks
        let (promise, recorded) = record::<()>();
        manager.send_call_log(
            &mut env,
            &Some(td_api::InputCall::inputCallDiscarded(
                td_api::inputCallDiscarded { call_id: 9 },
            )),
            None,
            promise,
        );
        assert!(recorded.0.lock().unwrap().is_none());
        assert_eq!(manager.pending.save_call_log.len(), 1);
    }

    #[test]
    fn signaling_data_gates_before_anything_parks() {
        let mut env = TestEnv::new();
        let mut manager = CallManager::new();

        // the unknown-call band errors before anything parks
        let (promise, recorded) = record::<()>();
        manager.send_call_signaling_data(&mut env, CallId::new(9), b"x".to_vec(), promise);
        assert!(matches!(
            recorded.0.lock().unwrap().take().unwrap(),
            Result::Err(ref status) if status.message() == "Call not found"
        ));

        // a fresh (Empty-state) actor takes the "Call is not active"
        // band before anything parks (cpp:273-275); the Ready-state
        // parking posture round-trips in the netted dispatcher test
        manager
            .id_to_actor
            .insert(CallId::new(3), CallActor::new(CallId::new(3)));
        let (promise, recorded) = record::<()>();
        manager.send_call_signaling_data(&mut env, CallId::new(3), b"x".to_vec(), promise);
        assert!(matches!(
            recorded.0.lock().unwrap().take().unwrap(),
            Result::Err(ref status) if status.message() == "Call is not active"
        ));
        assert!(manager.pending.send_signaling_data.is_empty());
        assert!(env.queries.is_empty());
    }
}
