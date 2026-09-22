//! `td::GroupCallManager` (GroupCallManager.h/.cpp, M.51) — the
//! signaling-only port (D4): the group-call value plane and the request
//! router with the C++ local validation. The media transport is deferred
//! by design; the query tails are [blocked: net] seams with parked
//! promises, and the E2E group-call key functions ride the tde2e port
//! (M.50) as [blocked: tde2e] seams.

use crate::dialog_id::DialogId;
use crate::store_parse::{self, Parser};
use crate::td_api;
use std::collections::HashMap;
use tdutils::promise::Promise;
use tdutils::status::Status;

/// `td::GroupCallId` (GroupCallId.h) — the input-group-call id space.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GroupCallId(i32);

impl GroupCallId {
    pub const fn new(id: i32) -> GroupCallId {
        GroupCallId(id)
    }

    pub const fn get(self) -> i32 {
        self.0
    }

    /// `GroupCallId::is_valid` (GroupCallId.h).
    pub const fn is_valid(self) -> bool {
        self.0 > 0
    }

    /// `GroupCallId::store` (GroupCallId.h).
    pub fn store(self, out: &mut Vec<u8>) {
        store_parse::store_i32(out, self.0);
    }

    /// `GroupCallId::parse` (GroupCallId.h).
    pub fn parse(parser: &mut Parser) -> std::result::Result<GroupCallId, &'static str> {
        Ok(GroupCallId::new(parser.i32_()?))
    }
}

/// `td::GroupCall` — the call record (the participant/media halves stay
/// on the deferred transport plane).
#[derive(Clone, Debug, Default)]
pub struct GroupCall {
    pub group_call_id: GroupCallId,
    pub chat_id: i64,
    pub title: String,
    pub is_active: bool,
    pub is_joined: bool,
    pub is_my_video_enabled: bool,
    pub is_my_video_paused: bool,
}

/// The `G()`/`td_` surface the group-call plane reads through. Every
/// query tail is a [blocked: net] seam with a parked promise; the
/// E2E-key functions are [blocked: tde2e].
pub trait GroupCallEnv {
    fn is_authorized(&self) -> bool;
    fn is_bot(&self) -> bool;

    fn send_update(&mut self, update: td_api::Update) {
        tdutils::log_info!("send_update({update:?}) — seam default");
    }

    fn on_get_dialog_error(&mut self, dialog_id: DialogId, status: Status, source: &str) {
        let _ = (dialog_id, status, source);
    }

    /// The generic query seam — the arm name + the parked promise.
    fn send_group_call_query(&mut self, name: &str, promise: Promise<()>) {
        tdutils::log_info!("Send {name} — [blocked: net]");
        let _ = promise;
    }
    fn send_group_call_query_text(&mut self, name: &str, promise: Promise<td_api::Text>) {
        tdutils::log_info!("Send {name} — [blocked: net]");
        let _ = promise;
    }
    fn send_group_call_query_group_call(
        &mut self,
        name: &str,
        promise: Promise<Option<td_api::GroupCall>>,
    ) {
        tdutils::log_info!("Send {name} — [blocked: net]");
        let _ = promise;
    }
    fn send_group_call_query_i64(&mut self, name: &str, promise: Promise<i64>) {
        tdutils::log_info!("Send {name} — [blocked: net]");
        let _ = promise;
    }
    fn send_group_call_query_message_senders(
        &mut self,
        name: &str,
        promise: Promise<td_api::MessageSenders>,
    ) {
        tdutils::log_info!("Send {name} — [blocked: net]");
        let _ = promise;
    }
    fn send_group_call_query_rtmp_url(&mut self, name: &str, promise: Promise<td_api::RtmpUrl>) {
        tdutils::log_info!("Send {name} — [blocked: net]");
        let _ = promise;
    }
    fn send_group_call_query_group_call_participants(
        &mut self,
        name: &str,
        promise: Promise<td_api::GroupCallParticipants>,
    ) {
        tdutils::log_info!("Send {name} — [blocked: net]");
        let _ = promise;
    }
    fn send_group_call_query_group_call_streams(
        &mut self,
        name: &str,
        promise: Promise<td_api::GroupCallStreams>,
    ) {
        tdutils::log_info!("Send {name} — [blocked: net]");
        let _ = promise;
    }
    fn send_group_call_query_star_amount(
        &mut self,
        name: &str,
        promise: Promise<td_api::StarAmount>,
    ) {
        tdutils::log_info!("Send {name} — [blocked: net]");
        let _ = promise;
    }
}

/// `td::GroupCallManager` (GroupCallManager.h) — signaling-only (D4).
#[derive(Default)]
pub struct GroupCallManager {
    group_calls: HashMap<GroupCallId, GroupCall>,
}

impl GroupCallManager {
    pub fn new() -> GroupCallManager {
        GroupCallManager::default()
    }

    pub fn get_group_call(&self, group_call_id: GroupCallId) -> Option<&GroupCall> {
        self.group_calls.get(&group_call_id)
    }

    /// The registry intake (GroupCallManager::on_get_group_call).
    pub fn on_get_group_call(&mut self, env: &mut impl GroupCallEnv, group_call: GroupCall) {
        let group_call_id = group_call.group_call_id;
        self.group_calls.insert(group_call_id, group_call);
        env.send_update(td_api::Update::updateGroupCall(td_api::updateGroupCall {
            group_call: None,
        }));
    }

    /// `check_group_call_id` — the shared id gate.
    pub fn check_group_call_id(group_call_id: GroupCallId) -> Status {
        if !group_call_id.is_valid() {
            return Status::error(400, "Invalid group call identifier specified");
        }
        Status::ok()
    }

    /// The generic request router for the [blocked: net] plane: the
    /// arm-specific validation happens in the request arms; here the
    /// promise parks in the seam.
    pub fn run_query(&mut self, env: &mut impl GroupCallEnv, name: &str, promise: Promise<()>) {
        env.send_group_call_query(name, promise);
    }

    /// `get_current_state` (Requests.cpp:2253).
    pub fn get_current_state(
        &mut self,
        env: &mut impl GroupCallEnv,
        updates: &mut Vec<td_api::Update>,
    ) {
        for group_call in self.group_calls.values() {
            let _ = group_call;
            env.send_update(td_api::Update::updateGroupCall(td_api::updateGroupCall {
                group_call: None,
            }));
        }
        let _ = updates;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_call_id_store_parse_round_trip() {
        let value = GroupCallId::new(42);
        let mut out = Vec::new();
        value.store(&mut out);
        let mut parser = Parser::new(&out);
        assert_eq!(GroupCallId::parse(&mut parser).unwrap(), value);
        assert!(value.is_valid());
        assert!(!GroupCallId::default().is_valid());
    }

    #[test]
    fn check_id_rejects_invalid() {
        assert_eq!(
            GroupCallManager::check_group_call_id(GroupCallId::new(1)).code(),
            0
        );
        assert_eq!(
            GroupCallManager::check_group_call_id(GroupCallId::default()).code(),
            400
        );
    }

    #[test]
    fn intake_registers_the_call() {
        let mut manager = GroupCallManager::new();
        let mut env = TestEnv::default();
        manager.on_get_group_call(
            &mut env,
            GroupCall {
                group_call_id: GroupCallId::new(5),
                ..GroupCall::default()
            },
        );
        assert!(manager.get_group_call(GroupCallId::new(5)).is_some());
    }

    /// The minimal test env.
    #[derive(Default)]
    struct TestEnv {
        updates: Vec<td_api::Update>,
    }

    impl GroupCallEnv for TestEnv {
        fn is_authorized(&self) -> bool {
            true
        }

        fn is_bot(&self) -> bool {
            false
        }

        fn send_update(&mut self, update: td_api::Update) {
            self.updates.push(update);
        }
    }
}
