//! `td::GroupCallManager` (GroupCallManager.h/.cpp, M.51) — the
//! signaling-only port (D4): the group-call value plane and the request
//! router with the C++ local validation. The media transport is deferred
//! by design (ticket td-rs/td#54 — the join/payload/participant planes);
//! the E2E group-call key functions ride the tde2e port (M.50) as
//! [blocked: tde2e] seams.
//!
//! The net-drain 36 slice (td-rs/td#45) replaces the generic `run_query`
//! park with the per-family net plane. The router arms audit — every
//! arm the C++ `Requests.cpp:5152-5445` routes into the manager, its
//! C++ tail and the port's wire family:
//!
//! | arm | C++ tail | wire family |
//! |---|---|---|
//! | getGroupCall | `reload_group_call` (cpp:2807) | `GetGroupCall` — phone.getGroupCall (cpp:450) |
//! | getLiveStoryStreamer | bands → `GetGroupCallStreamerQuery` (cpp:480) | `GetStreamer` — phone.getGroupCall limit 10 |
//! | getVideoChatAvailableParticipants | `get_group_call_join_as` (cpp:2443) | `GetJoinAs` — phone.getGroupCallJoinAs (cpp:151) |
//! | getLiveStoryAvailableMessageSenders | `get_group_call_send_as` → local `do_get_group_call_send_as` (cpp:2490-2582) | local — the cpp:2517 net leg is the update-driven reload |
//! | setVideoChatDefaultParticipant | `set_group_call_default_join_as` (cpp:2586) | `SaveJoinAs` — phone.saveDefaultGroupCallJoinAs (cpp:242) |
//! | setLiveStoryMessageSender | `set_group_call_default_send_as` (cpp:2615) | `SaveSendAs` — phone.saveDefaultSendAs (cpp:279) |
//! | startScheduledVideoChat | bands → `StartScheduledGroupCallQuery` (cpp:767) | `StartScheduled` — phone.startScheduledGroupCall |
//! | toggleVideoChatEnabledStartNotification | bands + eq settle → `ToggleGroupCallStartSubscriptionQuery` (cpp:992) | `ToggleStartSubscription` — phone.toggleGroupCallStartSubscription |
//! | setVideoChatTitle | bands + eq settle → `EditGroupCallTitleQuery` (cpp:959) | `EditTitle` — phone.editGroupCallTitle |
//! | toggleVideoChatMuteNewParticipants | bands + eq settle → `ToggleGroupCallSettingsQuery` (cpp:1025) | `ToggleSettings` — phone.toggleGroupCallSettings |
//! | toggleGroupCallAreMessagesAllowed | bands + eq settle (cpp:4180) | `ToggleSettings` |
//! | setGroupCallPaidMessageStarCount | bands + eq settle (cpp:4266) | `ToggleSettings` |
//! | revokeGroupCallInviteLink | band → the reset_invite_hash form (cpp:1180) | `ToggleSettings` |
//! | getVideoChatRtmpUrl / replaceVideoChatRtmpUrl / getLiveStoryRtmpUrl / replaceLiveStoryRtmpUrl | `get_video_chat_rtmp_stream_url` (cpp:2781) | `GetRtmpUrl` — phone.getGroupCallStreamRtmpUrl (cpp:415) |
//! | endGroupCallScreenSharing | bands → `LeaveGroupCallPresentationQuery` (cpp:926) | `LeavePresentation` — phone.leaveGroupCallPresentation |
//! | startGroupCallRecording / endGroupCallRecording | `toggle_group_call_recording` (cpp:1176) | `ToggleRecord` — phone.toggleGroupCallRecord (cpp:1442) |
//! | getVideoChatInviteLink | bands → `ExportGroupCallInviteQuery` (cpp:1415) | `ExportInvite` — phone.exportGroupCallInvite |
//! | inviteVideoChatParticipants / inviteGroupCallParticipant | `invite_group_call_participants` (cpp:3895) | `Invite` — phone.inviteToGroupCall (cpp:1387) |
//! | declineGroupCallInvitation | `decline_group_call_invitation` (cpp:1316) | `DeclineInvite` — phone.declineConferenceCallInvite (cpp:1321) |
//! | sendGroupCallMessage | bands → `SendGroupCallMessageQuery` (cpp:1071) | `SendMessage` — phone.sendGroupCallMessage |
//! | deleteGroupCallMessages | bands → the call-message store (media plane) | local — the cpp:1171 net leg rides the stored messages |
//! | deleteGroupCallMessagesBySender | bands → `DeleteGroupCallParticipantMessagesQuery` (cpp:1201) | `DeleteSenderMessages` — phone.deleteGroupCallParticipantMessages |
//! | loadGroupCallParticipants | bands → `GetGroupCallParticipantsQuery` (cpp:643) | `GetParticipants` — phone.getGroupParticipants |
//! | getLiveStoryTopDonors | bands → `GetGroupCallStarsQuery` (cpp:1233) | `GetStars` — phone.getGroupCallStars |
//! | getGroupCallStreams | bands → `GetGroupCallStreamChannelsQuery` (cpp:76) | `GetStreams` — phone.getGroupCallStreamChannels |
//! | leaveGroupCall | bands → `LeaveGroupCallQuery` (cpp:1572) | `Leave` — phone.leaveGroupCall |
//! | endGroupCall | `discard_group_call` (cpp:7255) | `Discard` — phone.discardGroupCall |
//! | banGroupCallParticipants | self band + the conference band (cpp:3878); the conference net leg is `DeleteConferenceCallParticipantsQuery` (tde2e plane) | local — tde2e-deferred |
//! | toggleGroupCallScreenSharingIsPaused | local pending-flag settle (cpp:3960) | local |
//! | toggleGroupCallIsMyVideoPaused | local settle (cpp:4020) | local |
//! | toggleGroupCallIsMyVideoEnabled | local settle (cpp:4077) | local |
//! | addPendingLiveStoryReaction | `send_group_call_reaction` bands + pending bookkeeping (cpp:5980) | local |
//! | commitPendingLiveStoryReactions | `commit_pending_group_call_reactions` (cpp:6046) — the aggregated `SendGroupCallMessageQuery` rides the pending-reaction store | local — media-plane store |
//! | removePendingLiveStoryReactions | `remove_pending_group_call_reactions` (cpp:6109) | local |
//! | createVideoChat / createGroupCall | the join-payload flow (cpp:2658, 2679) | parked — the media plane |
//! | joinGroupCall / joinVideoChat / joinLiveStory | the join flow (cpp:4434, 4530) | parked — the media plane |
//! | startGroupCallScreenSharing | `JoinGroupCallPresentationQuery` (cpp:891) | parked — the media plane |
//! | toggleGroupCallParticipantIsMuted / toggleGroupCallParticipantIsHandRaised / setGroupCallParticipantVolumeLevel | the participant plane (cpp:6779, 6886, 6986) | parked — the participants plane |

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
/// on the deferred transport plane). The net-drain 36 slice adds the
/// fields the router bands read (the C++ `GroupCall` state slice,
/// GroupCall.h:39-419).
#[derive(Clone, Debug, Default)]
pub struct GroupCall {
    pub group_call_id: GroupCallId,
    /// The wire `InputGroupCallId` pair (the C++ `input_group_call_ids_`
    /// registration) — the phone.* functions carry it.
    pub input_group_call_id: i64,
    pub input_group_call_access_hash: i64,
    pub chat_id: i64,
    pub title: String,
    pub is_active: bool,
    pub is_joined: bool,
    pub is_my_video_enabled: bool,
    pub is_my_video_paused: bool,
    /// `group_call->is_conference` (GroupCall.h:63).
    pub is_conference: bool,
    /// `group_call->is_live_story` (GroupCall.h:74).
    pub is_live_story: bool,
    /// `group_call->can_be_managed` (GroupCall.h:115).
    pub can_be_managed: bool,
    /// `group_call->scheduled_start_date` (GroupCall.h:82).
    pub scheduled_start_date: i32,
    /// `group_call->mute_new_participants` (GroupCall.h:104).
    pub mute_new_participants: bool,
    /// `group_call->are_messages_enabled` (GroupCall.h:106).
    pub are_messages_enabled: bool,
    /// `group_call->paid_message_star_count` (GroupCall.h:107).
    pub paid_message_star_count: i64,
    /// `group_call->start_subscribed` (GroupCall.h:84).
    pub start_subscribed: bool,
    /// `group_call->is_video_recorded` (GroupCall.h:118).
    pub is_video_recorded: bool,
    /// `group_call->stream_dc_id` (GroupCall.h:70) — 0 marks the
    /// non-exact id (the C++ `stream_dc_id.is_exact()` band).
    pub stream_dc_id: i32,
    /// `group_call->audio_source` (GroupCall.h:161) — the media-plane
    /// source id; 0 when the join has not run.
    pub audio_source: i32,
    /// `group_call->message_sender_dialog_id` (GroupCall.h:96) — the
    /// live-story comment sender.
    pub message_sender_dialog_id: DialogId,
}

impl GroupCall {
    /// The wire `telegram_api::inputGroupCall` of the record.
    fn input_group_call(&self) -> tdmtproto::telegram_api::InputGroupCall {
        tdmtproto::telegram_api::InputGroupCall::inputGroupCall(
            tdmtproto::telegram_api::inputGroupCall {
                id: self.input_group_call_id,
                access_hash: self.input_group_call_access_hash,
            },
        )
    }
}

/// The `G()`/`td_` surface the group-call plane reads through. The query
/// sends collapse onto the manager-held `GroupCallNetHandle`
/// (the `None` keeps the net-inert posture); the E2E-key functions are
/// [blocked: tde2e].
pub trait GroupCallEnv {
    fn is_authorized(&self) -> bool;
    fn is_bot(&self) -> bool;

    fn send_update(&mut self, update: td_api::Update) {
        tdutils::log_info!("send_update({update:?}) — seam default");
    }

    fn on_get_dialog_error(&mut self, dialog_id: DialogId, status: Status, source: &str) {
        let _ = (dialog_id, status, source);
    }

    /// `DialogManager::get_input_peer(dialog_id, Read)` — the send-leg
    /// peer resolution of the peer-bearing queries (cpp:166, 247, 288,
    /// 421). The narrow env views can't reach the dialog plane, so the
    /// default answers `None` (the C++ `CHECK(input_peer != nullptr)`
    /// is fatal; the port downgrades it to a non-fatal drop band, the
    /// nd32 seam convention).
    fn get_input_peer_read(
        &mut self,
        _dialog_id: DialogId,
    ) -> Option<tdmtproto::telegram_api::InputPeer> {
        None
    }
}

/// The GroupCallManager in-flight query kinds — the C++ ResultHandler
/// classes of the net-drain 36 families (GroupCallManager.cpp:76-1630).
/// The handler state the answers read (dialog ids, the request api id,
/// the star ledger inputs) rides the [`PendingGroupCallQueries`] lots —
/// the kinds stay `Copy`.
#[derive(Clone, Copy)]
pub(crate) enum GroupCallNetKind {
    /// `GetGroupCallQuery` (cpp:450) — `phone.getGroupCall`; the reload
    /// leg the getGroupCall request rides (the api id keys the registry
    /// update).
    GetGroupCall { api_group_call_id: i32 },
    /// `GetGroupCallStreamerQuery` (cpp:480) — the streamer scan.
    GetStreamer,
    /// `GetGroupCallJoinAsQuery` (cpp:151) — the join-as list; the error
    /// ladder keys the request chat.
    GetJoinAs { dialog_id: DialogId },
    /// `GetGroupCallStreamRtmpUrlQuery` (cpp:415) — the error ladder
    /// keys the request chat.
    GetRtmpUrl { dialog_id: DialogId },
    /// `SaveDefaultGroupCallJoinAsQuery` (cpp:242).
    SaveJoinAs,
    /// `SaveDefaultGroupCallSendAsQuery` (cpp:279) — the as-dialog band.
    SaveSendAs { as_dialog_id: DialogId },
    /// `StartScheduledGroupCallQuery` (cpp:767).
    StartScheduled,
    /// `ToggleGroupCallStartSubscriptionQuery` (cpp:992).
    ToggleStartSubscription,
    /// `EditGroupCallTitleQuery` (cpp:959).
    EditTitle,
    /// `ToggleGroupCallSettingsQuery` (cpp:1025) — the
    /// mute-new/messages/paid-star-count/revoke arms share the class.
    ToggleSettings,
    /// `LeaveGroupCallPresentationQuery` (cpp:926).
    LeavePresentation,
    /// `ToggleGroupCallRecordQuery` (cpp:1442).
    ToggleRecord,
    /// `ExportGroupCallInviteQuery` (cpp:1415).
    ExportInvite,
    /// `InviteToGroupCallQuery` (cpp:1387).
    Invite,
    /// `DeclineConferenceCallInviteQuery` (cpp:1321).
    DeclineInvite,
    /// `SendGroupCallMessageQuery` (cpp:1071) — the send/comment leg;
    /// the answer runs the star ledger intake (cpp:1077-1079).
    SendMessage { paid_message_star_count: i64 },
    /// `DeleteGroupCallParticipantMessagesQuery` (cpp:1201) — the error
    /// ladder keys the sender dialog.
    DeleteSenderMessages { sender_dialog_id: DialogId },
    /// `GetGroupCallParticipantsQuery` (cpp:643).
    GetParticipants,
    /// `GetGroupCallStarsQuery` (cpp:1233) — the donors leg; the promise
    /// lot is FIFO-coalesced like the C++ `get_stars_queries_`.
    GetStars,
    /// `GetGroupCallStreamChannelsQuery` (cpp:76).
    GetStreams,
    /// `LeaveGroupCallQuery` (cpp:1572).
    Leave,
    /// `DiscardGroupCallQuery` (cpp:1601).
    Discard,
}

/// The parked query promises — the ResultHandler members the `Copy`
/// [`GroupCallNetKind`]s can't carry (the typed promises). FIFO-paired
/// per family (§5).
#[derive(Default)]
pub(crate) struct PendingGroupCallQueries {
    /// `GetGroupCallQuery::promise_` — the reload settles the typed
    /// getGroupCall promise; the request api id rides the lot.
    pub(crate) group_call: Vec<(i32, Promise<Option<td_api::GroupCall>>)>,
    /// `GetGroupCallStreamerQuery::promise_`.
    pub(crate) streamer: Vec<Promise<Option<td_api::GroupCallParticipant>>>,
    /// `GetGroupCallJoinAsQuery::promise_`.
    pub(crate) join_as: Vec<Promise<td_api::MessageSenders>>,
    /// `GetGroupCallStreamRtmpUrlQuery::promise_`.
    pub(crate) rtmp_url: Vec<Promise<td_api::RtmpUrl>>,
    /// `SaveDefaultGroupCallJoinAsQuery::promise_`.
    pub(crate) save_join_as: Vec<Promise<()>>,
    /// `SaveDefaultGroupCallSendAsQuery::promise_`.
    pub(crate) save_send_as: Vec<Promise<()>>,
    /// `StartScheduledGroupCallQuery::promise_`.
    pub(crate) start_scheduled: Vec<Promise<()>>,
    /// `ToggleGroupCallStartSubscriptionQuery::promise_`.
    pub(crate) toggle_start_subscription: Vec<Promise<()>>,
    /// `EditGroupCallTitleQuery::promise_` — the request settles before
    /// the send (cpp:5231), so the lot parks the query-side promise.
    pub(crate) edit_title: Vec<Promise<()>>,
    /// `ToggleGroupCallSettingsQuery::promise_`.
    pub(crate) toggle_settings: Vec<Promise<()>>,
    /// `LeaveGroupCallPresentationQuery::promise_`.
    pub(crate) leave_presentation: Vec<Promise<()>>,
    /// `ToggleGroupCallRecordQuery::promise_`.
    pub(crate) toggle_record: Vec<Promise<()>>,
    /// `ExportGroupCallInviteQuery::promise_` — the invite link.
    pub(crate) export_invite: Vec<Promise<String>>,
    /// `InviteToGroupCallQuery::promise_`.
    pub(crate) invite: Vec<Promise<()>>,
    /// `DeclineConferenceCallInviteQuery::promise_`.
    pub(crate) decline_invite: Vec<Promise<()>>,
    /// `SendGroupCallMessageQuery::promise_`.
    pub(crate) send_message: Vec<Promise<()>>,
    /// `DeleteGroupCallParticipantMessagesQuery::promise_`.
    pub(crate) delete_sender_messages: Vec<Promise<()>>,
    /// `GetGroupCallParticipantsQuery::promise_`.
    pub(crate) get_participants: Vec<Promise<()>>,
    /// `GetGroupCallStarsQuery::promise_` — the donors object.
    pub(crate) get_stars: Vec<Promise<td_api::LiveStoryDonors>>,
    /// `GetGroupCallStreamChannelsQuery::promise_`.
    pub(crate) get_streams: Vec<Promise<td_api::GroupCallStreams>>,
    /// `LeaveGroupCallQuery::promise_`.
    pub(crate) leave: Vec<Promise<()>>,
    /// `DiscardGroupCallQuery::promise_`.
    pub(crate) discard: Vec<Promise<()>>,
}

/// `td::GroupCallManager` (GroupCallManager.h) — signaling-only (D4).
#[derive(Default)]
pub(crate) struct GroupCallManager {
    pub(crate) group_calls: HashMap<GroupCallId, GroupCall>,
    /// The parked query promises of the net-drain 36 families.
    pub(crate) pending: PendingGroupCallQueries,
    /// The net dispatch surface — the `td_` back-pointer the C++ manager
    /// uses for its query sends, narrowed to the net plane (`None` in
    /// the net-inert tests; set at the manager install, §0's shared-env
    /// shape).
    pub(crate) net: Option<crate::td::GroupCallNetHandle>,
}

/// `ChatManager::MAX_TITLE_LENGTH` (ChatManager.h:678) — the title
/// clean-up bound.
const MAX_TITLE_LENGTH: usize = 128;

/// The `ToggleGroupCallSettingsQuery` field pair — the four arms share
/// the class; each arm sets one field (the C++ flag-mask set:
/// JOIN_MUTED_MASK = 1, RESET_INVITE_HASH_MASK = 2,
/// MESSAGES_ENABLED_MASK = 4, SEND_PAID_MESSAGES_STARS_MASK = 8).
enum ToggleSettingsForm {
    /// `toggle_group_call_mute_new_participants` (cpp:5617).
    JoinMuted(bool),
    /// `revoke_group_call_invite_link` (cpp:6429).
    ResetInviteHash,
    /// `toggle_group_call_are_messages_enabled` (cpp:5704).
    MessagesEnabled(bool),
    /// `set_group_call_paid_message_star_count` (cpp:5789).
    PaidMessageStarCount(i64),
}

impl GroupCallManager {
    pub(crate) fn new() -> GroupCallManager {
        GroupCallManager::default()
    }

    pub(crate) fn get_group_call(&self, group_call_id: GroupCallId) -> Option<&GroupCall> {
        self.group_calls.get(&group_call_id)
    }

    /// The registry intake (GroupCallManager::on_get_group_call) — the
    /// `update_group_call` refire rides the updates plane (net-drain
    /// 37); unused until that refire lands, hence the allow.
    #[allow(dead_code)]
    pub(crate) fn on_get_group_call(&mut self, env: &mut impl GroupCallEnv, group_call: GroupCall) {
        let group_call_id = group_call.group_call_id;
        self.group_calls.insert(group_call_id, group_call);
        env.send_update(td_api::Update::updateGroupCall(td_api::updateGroupCall {
            group_call: None,
        }));
    }

    /// `check_group_call_id` — the shared id gate.
    pub(crate) fn check_group_call_id(group_call_id: GroupCallId) -> Status {
        if !group_call_id.is_valid() {
            return Status::error(400, "Invalid group call identifier specified");
        }
        Status::ok()
    }

    /// `get_input_group_call_id` (cpp:2283-2292) — the id gate plus the
    /// registration lookup. The port holds the api id space directly:
    /// an unregistered id is the C++ "Wrong group call identifier
    /// specified" band.
    fn resolve_group_call(
        &self,
        promise: &mut Promise<()>,
        group_call_id: GroupCallId,
    ) -> Option<GroupCall> {
        Self::resolve_group_call_typed(self, promise, group_call_id)
    }

    /// The typed-promise form of the [`GroupCallManager::resolve_group_call`]
    /// gate — `check_group_call_id` (cpp:2283-2286) plus the
    /// registration lookup (the unregistered band, cpp:2289).
    fn resolve_group_call_typed<E: Send + 'static>(
        &self,
        promise: &mut Promise<E>,
        group_call_id: GroupCallId,
    ) -> Option<GroupCall> {
        let status = Self::check_group_call_id(group_call_id);
        if status.code() != 0 {
            promise.set_error(status);
            return None;
        }
        match self.group_calls.get(&group_call_id) {
            Some(group_call) => Some(group_call.clone()),
            None => {
                promise.set_error(Status::error(400, "Wrong group call identifier specified"));
                None
            }
        }
    }

    /// The `td_->create_handler<…>()->send(...)` collapse — the wire
    /// query construction lives at the manager send sites; `None` keeps
    /// the net-inert posture (the tests drive the answer halves).
    fn send_group_call_net_query(
        &self,
        kind: GroupCallNetKind,
        function: tdmtproto::telegram_api::Function,
        what: &str,
    ) {
        match &self.net {
            Some(net) => net.dispatch(kind, function),
            None => tdutils::log_info!("Send {what} — [net-inert]"),
        }
    }

    /// `get_group_call` (cpp:2807-2817) — the request reloads the call
    /// through `GetGroupCallQuery` (the C++ reload_group_call chain;
    /// the port settles the promise from the answer).
    pub(crate) fn reload_group_call(
        &mut self,
        group_call_id: GroupCallId,
        promise: Promise<Option<td_api::GroupCall>>,
    ) {
        let mut promise = promise;
        let Some(group_call) = self.resolve_group_call_typed(&mut promise, group_call_id) else {
            return;
        };
        self.pending.group_call.push((group_call_id.get(), promise));
        self.send_group_call_net_query(
            GroupCallNetKind::GetGroupCall {
                api_group_call_id: group_call_id.get(),
            },
            tdmtproto::telegram_api::Function::phone_getGroupCall(
                tdmtproto::telegram_api::phone_getGroupCall {
                    call: Some(group_call.input_group_call()),
                    limit: 3,
                },
            ),
            "phone.getGroupCall",
        );
    }

    /// `get_group_call_streamer` (cpp:2450-2479) — the streamer scan
    /// over the phone.getGroupCall answer.
    pub(crate) fn get_group_call_streamer(
        &mut self,
        group_call_id: GroupCallId,
        promise: Promise<Option<td_api::GroupCallParticipant>>,
    ) {
        let mut promise = promise;
        let Some(group_call) = self.resolve_group_call_typed(&mut promise, group_call_id) else {
            return;
        };
        if !group_call.is_active {
            promise.set_error(Status::error(400, "GROUPCALL_JOIN_MISSING"));
            return;
        }
        if group_call.is_conference || group_call.is_live_story {
            promise.set_value(None);
            return;
        }
        self.pending.streamer.push(promise);
        self.send_group_call_net_query(
            GroupCallNetKind::GetStreamer,
            tdmtproto::telegram_api::Function::phone_getGroupCall(
                tdmtproto::telegram_api::phone_getGroupCall {
                    call: Some(group_call.input_group_call()),
                    limit: 10,
                },
            ),
            "phone.getGroupCall (streamer)",
        );
    }

    /// `get_group_call_join_as` (cpp:2443-2449) — the join-as list of
    /// the request chat. The input peer resolves through the env seam;
    /// a `None` peer drops the send (the nd32 convention).
    pub(crate) fn get_group_call_join_as(
        &mut self,
        env: &mut impl GroupCallEnv,
        dialog_id: DialogId,
        promise: Promise<td_api::MessageSenders>,
    ) {
        let mut promise = promise;
        if !dialog_id.is_valid() {
            promise.set_error(Status::error(400, "Invalid chat identifier specified"));
            return;
        }
        let Some(peer) = env.get_input_peer_read(dialog_id) else {
            promise.set_error(Status::error(400, "Have no access to the chat"));
            return;
        };
        self.pending.join_as.push(promise);
        self.send_group_call_net_query(
            GroupCallNetKind::GetJoinAs { dialog_id },
            tdmtproto::telegram_api::Function::phone_getGroupCallJoinAs(
                tdmtproto::telegram_api::phone_getGroupCallJoinAs {
                    peer: Some(Box::new(peer)),
                },
            ),
            "phone.getGroupCallJoinAs",
        );
    }

    /// `get_group_call_send_as` (cpp:2490-2582) — the local sender-list
    /// recipe (the cpp:2517 net leg is the update-driven reload). The
    /// bands return the dialog ids in the C++ add order; the sender
    /// objects build at the request arm over the full `Td` frame.
    pub(crate) fn group_call_send_as_dialog_ids(
        &self,
        group_call_id: GroupCallId,
        my_dialog_id: DialogId,
        created_public_broadcasts: &[i64],
    ) -> Result<Vec<DialogId>, Status> {
        let group_call = self
            .group_calls
            .get(&group_call_id)
            .ok_or_else(|| Status::error(400, "Wrong group call identifier specified"))?;
        if !group_call.is_active {
            return Err(Status::error(400, "GROUPCALL_JOIN_MISSING"));
        }
        let dialog_id = DialogId::new(group_call.chat_id);
        if !group_call.is_live_story || !dialog_id.is_valid() {
            return Ok(Vec::new());
        }
        if !group_call.is_joined {
            return Err(Status::error(400, "GROUPCALL_JOIN_MISSING"));
        }
        let mut dialog_ids = Vec::new();
        if dialog_id.get_type() == crate::dialog_id::DialogType::Channel
            && group_call.can_be_managed
        {
            dialog_ids.push(dialog_id);
        }
        if group_call.are_messages_enabled || group_call.can_be_managed {
            dialog_ids.push(my_dialog_id);
        }
        if group_call.are_messages_enabled && group_call.can_be_managed {
            for &channel_id in created_public_broadcasts {
                let channel_dialog_id =
                    DialogId::from_channel_id(crate::chat_manager::ChannelId::new(channel_id));
                if channel_dialog_id != dialog_id {
                    dialog_ids.push(channel_dialog_id);
                }
            }
        }
        Ok(dialog_ids)
    }

    /// `set_group_call_default_join_as` (cpp:2586-2613) — the dialog
    /// bands resolve at the request arm (the port's dialog reads run
    /// over `Td`, outside the manager frame); the tail dispatches.
    pub(crate) fn set_group_call_default_join_as(
        &mut self,
        env: &mut impl GroupCallEnv,
        dialog_id: DialogId,
        join_as_peer: tdmtproto::telegram_api::InputPeer,
        promise: Promise<()>,
    ) {
        let mut promise = promise;
        if !dialog_id.is_valid() {
            promise.set_error(Status::error(400, "Invalid chat identifier specified"));
            return;
        }
        let Some(peer) = env.get_input_peer_read(dialog_id) else {
            promise.set_error(Status::error(400, "Can't access the chat"));
            return;
        };
        self.pending.save_join_as.push(promise);
        self.send_group_call_net_query(
            GroupCallNetKind::SaveJoinAs,
            tdmtproto::telegram_api::Function::phone_saveDefaultGroupCallJoinAs(
                tdmtproto::telegram_api::phone_saveDefaultGroupCallJoinAs {
                    peer: Some(Box::new(peer)),
                    join_as: Some(Box::new(join_as_peer)),
                },
            ),
            "phone.saveDefaultGroupCallJoinAs",
        );
    }

    /// `set_group_call_default_send_as` (cpp:2615-2656) — the call
    /// bands + the equality settle; the as-dialog access check and the
    /// another-user band resolve at the request arm (`is_another_user`).
    pub(crate) fn set_group_call_default_send_as(
        &mut self,
        env: &mut impl GroupCallEnv,
        group_call_id: GroupCallId,
        as_dialog_id: DialogId,
        is_another_user: bool,
        as_peer: tdmtproto::telegram_api::InputPeer,
        promise: Promise<()>,
    ) {
        let mut promise = promise;
        let Some(mut group_call) = self.resolve_group_call(&mut promise, group_call_id) else {
            return;
        };
        if !group_call.is_active {
            promise.set_error(Status::error(400, "GROUPCALL_JOIN_MISSING"));
            return;
        }
        if !group_call.is_live_story {
            promise.set_error(Status::error(
                400,
                "Group call message sender can't be set explicitly",
            ));
            return;
        }
        if !group_call.is_joined {
            promise.set_error(Status::error(400, "GROUPCALL_JOIN_MISSING"));
            return;
        }
        if is_another_user {
            promise.set_error(Status::error(
                400,
                "Can't send live story comments as another user",
            ));
            return;
        }
        if group_call.message_sender_dialog_id == as_dialog_id {
            promise.set_value(());
            return;
        }
        group_call.message_sender_dialog_id = as_dialog_id;
        self.group_calls.insert(group_call_id, group_call.clone());
        self.send_update_group_call(env, &group_call);

        self.pending.save_send_as.push(promise);
        self.send_group_call_net_query(
            GroupCallNetKind::SaveSendAs { as_dialog_id },
            tdmtproto::telegram_api::Function::phone_saveDefaultSendAs(
                tdmtproto::telegram_api::phone_saveDefaultSendAs {
                    call: Some(group_call.input_group_call()),
                    send_as: Some(Box::new(as_peer)),
                },
            ),
            "phone.saveDefaultSendAs",
        );
    }

    /// `start_scheduled_group_call` (cpp:4400-4432).
    pub(crate) fn start_scheduled_group_call(
        &mut self,
        group_call_id: GroupCallId,
        promise: Promise<()>,
    ) {
        let mut promise = promise;
        let Some(group_call) = self.resolve_group_call(&mut promise, group_call_id) else {
            return;
        };
        if group_call.is_conference || group_call.is_live_story {
            promise.set_error(Status::error(400, "The group call isn't scheduled"));
            return;
        }
        if !group_call.can_be_managed {
            promise.set_error(Status::error(
                400,
                "Not enough rights to start the group call",
            ));
            return;
        }
        if !group_call.is_active {
            promise.set_error(Status::error(400, "Group call already ended"));
            return;
        }
        if group_call.scheduled_start_date == 0 {
            promise.set_value(());
            return;
        }
        self.pending.start_scheduled.push(promise);
        self.send_group_call_net_query(
            GroupCallNetKind::StartScheduled,
            tdmtproto::telegram_api::Function::phone_startScheduledGroupCall(
                tdmtproto::telegram_api::phone_startScheduledGroupCall {
                    call: Some(group_call.input_group_call()),
                },
            ),
            "phone.startScheduledGroupCall",
        );
    }

    /// `toggle_group_call_start_subscribed` (cpp:4349-4398).
    pub(crate) fn toggle_group_call_start_subscribed(
        &mut self,
        env: &mut impl GroupCallEnv,
        group_call_id: GroupCallId,
        start_subscribed: bool,
        promise: Promise<()>,
    ) {
        let mut promise = promise;
        let Some(mut group_call) = self.resolve_group_call(&mut promise, group_call_id) else {
            return;
        };
        if group_call.is_conference
            || !group_call.is_active
            || group_call.scheduled_start_date <= 0
            || group_call.is_live_story
        {
            promise.set_error(Status::error(400, "The group call isn't scheduled"));
            return;
        }
        if start_subscribed == group_call.start_subscribed {
            promise.set_value(());
            return;
        }
        group_call.start_subscribed = start_subscribed;
        self.group_calls.insert(group_call_id, group_call.clone());
        self.send_update_group_call(env, &group_call);
        // there is no reason to save promise; we will send an update
        // with actual value anyway (cpp:4387)
        promise.set_value(());
        self.pending
            .toggle_start_subscription
            .push(Promise::default());
        self.send_group_call_net_query(
            GroupCallNetKind::ToggleStartSubscription,
            tdmtproto::telegram_api::Function::phone_toggleGroupCallStartSubscription(
                tdmtproto::telegram_api::phone_toggleGroupCallStartSubscription {
                    call: Some(group_call.input_group_call()),
                    subscribed: start_subscribed,
                },
            ),
            "phone.toggleGroupCallStartSubscription",
        );
    }

    /// `set_group_call_title` (cpp:5193-5236) — the title band + the
    /// equality settle + the pending-title update emission.
    pub(crate) fn set_group_call_title(
        &mut self,
        env: &mut impl GroupCallEnv,
        group_call_id: GroupCallId,
        title: String,
        promise: Promise<()>,
    ) {
        let mut promise = promise;
        let Some(mut group_call) = self.resolve_group_call(&mut promise, group_call_id) else {
            return;
        };
        if group_call.is_conference
            || !group_call.is_active
            || !group_call.can_be_managed
            || group_call.is_live_story
        {
            promise.set_error(Status::error(400, "Can't change group call title"));
            return;
        }

        let title = crate::misc::clean_name(&title, MAX_TITLE_LENGTH);
        if title == group_call.title {
            promise.set_value(());
            return;
        }

        group_call.title = title;
        self.group_calls.insert(group_call_id, group_call.clone());
        self.send_update_group_call(env, &group_call);
        // there is no reason to save promise; we will send an update
        // with actual value anyway (cpp:5231)
        promise.set_value(());
        self.pending.edit_title.push(Promise::default());
        self.send_group_call_net_query(
            GroupCallNetKind::EditTitle,
            tdmtproto::telegram_api::Function::phone_editGroupCallTitle(
                tdmtproto::telegram_api::phone_editGroupCallTitle {
                    call: Some(group_call.input_group_call()),
                    title: group_call.title.clone(),
                },
            ),
            "phone.editGroupCallTitle",
        );
    }

    /// `toggle_group_call_mute_new_participants` (cpp:5617-5655) — the
    /// shared ToggleGroupCallSettings leg (the can-manage band resolves
    /// at the request arm).
    pub(crate) fn toggle_group_call_mute_new_participants(
        &mut self,
        env: &mut impl GroupCallEnv,
        group_call_id: GroupCallId,
        mute_new_participants: bool,
        promise: Promise<()>,
    ) {
        let mut promise = promise;
        let Some(mut group_call) = self.resolve_group_call(&mut promise, group_call_id) else {
            return;
        };
        if !group_call.is_active {
            promise.set_error(Status::error(400, "GROUPCALL_JOIN_MISSING"));
            return;
        }
        if mute_new_participants == group_call.mute_new_participants {
            promise.set_value(());
            return;
        }
        group_call.mute_new_participants = mute_new_participants;
        self.group_calls.insert(group_call_id, group_call.clone());
        self.send_update_group_call(env, &group_call);
        promise.set_value(());
        self.pending.toggle_settings.push(Promise::default());
        self.send_group_call_net_query(
            GroupCallNetKind::ToggleSettings,
            Self::toggle_settings_function(
                &group_call,
                ToggleSettingsForm::JoinMuted(mute_new_participants),
            ),
            "phone.toggleGroupCallSettings (join_muted)",
        );
    }

    /// `toggle_group_call_are_messages_enabled` (cpp:5704-5741).
    pub(crate) fn toggle_group_call_are_messages_enabled(
        &mut self,
        env: &mut impl GroupCallEnv,
        group_call_id: GroupCallId,
        are_messages_enabled: bool,
        promise: Promise<()>,
    ) {
        let mut promise = promise;
        let Some(mut group_call) = self.resolve_group_call(&mut promise, group_call_id) else {
            return;
        };
        if !group_call.is_active {
            promise.set_error(Status::error(400, "GROUPCALL_JOIN_MISSING"));
            return;
        }
        if are_messages_enabled == group_call.are_messages_enabled {
            promise.set_value(());
            return;
        }
        group_call.are_messages_enabled = are_messages_enabled;
        self.group_calls.insert(group_call_id, group_call.clone());
        self.send_update_group_call(env, &group_call);
        promise.set_value(());
        self.pending.toggle_settings.push(Promise::default());
        self.send_group_call_net_query(
            GroupCallNetKind::ToggleSettings,
            Self::toggle_settings_function(
                &group_call,
                ToggleSettingsForm::MessagesEnabled(are_messages_enabled),
            ),
            "phone.toggleGroupCallSettings (messages_enabled)",
        );
    }

    /// `set_group_call_paid_message_star_count` (cpp:5789-5826).
    pub(crate) fn set_group_call_paid_message_star_count(
        &mut self,
        env: &mut impl GroupCallEnv,
        group_call_id: GroupCallId,
        paid_message_star_count: i64,
        promise: Promise<()>,
    ) {
        let mut promise = promise;
        let Some(mut group_call) = self.resolve_group_call(&mut promise, group_call_id) else {
            return;
        };
        if !group_call.is_active {
            promise.set_error(Status::error(400, "GROUPCALL_JOIN_MISSING"));
            return;
        }
        if paid_message_star_count == group_call.paid_message_star_count {
            promise.set_value(());
            return;
        }
        group_call.paid_message_star_count = paid_message_star_count;
        self.group_calls.insert(group_call_id, group_call.clone());
        self.send_update_group_call(env, &group_call);
        promise.set_value(());
        self.pending.toggle_settings.push(Promise::default());
        self.send_group_call_net_query(
            GroupCallNetKind::ToggleSettings,
            Self::toggle_settings_function(
                &group_call,
                ToggleSettingsForm::PaidMessageStarCount(paid_message_star_count),
            ),
            "phone.toggleGroupCallSettings (send_paid_messages_stars)",
        );
    }

    /// `revoke_group_call_invite_link` (cpp:6429-6431) — the
    /// reset_invite_hash form of ToggleGroupCallSettingsQuery.
    pub(crate) fn revoke_group_call_invite_link(
        &mut self,
        group_call_id: GroupCallId,
        promise: Promise<()>,
    ) {
        let mut promise = promise;
        let Some(group_call) = self.resolve_group_call(&mut promise, group_call_id) else {
            return;
        };
        if !group_call.can_be_managed {
            promise.set_error(Status::error(
                400,
                "Can't revoke invite link in the group call",
            ));
            return;
        }
        self.pending.toggle_settings.push(promise);
        self.send_group_call_net_query(
            GroupCallNetKind::ToggleSettings,
            Self::toggle_settings_function(&group_call, ToggleSettingsForm::ResetInviteHash),
            "phone.toggleGroupCallSettings (reset_invite_hash)",
        );
    }

    /// The `ToggleGroupCallSettingsQuery::send` wire form
    /// (cpp:1029-1049): the flag masks + the field pair.
    fn toggle_settings_function(
        group_call: &GroupCall,
        form: ToggleSettingsForm,
    ) -> tdmtproto::telegram_api::Function {
        let (flags, join_muted, messages_enabled, paid_message_star_count) = match form {
            ToggleSettingsForm::JoinMuted(value) => (1, Some(value), None, None),
            ToggleSettingsForm::ResetInviteHash => (2, None, None, None),
            ToggleSettingsForm::MessagesEnabled(value) => (4, None, Some(value), None),
            ToggleSettingsForm::PaidMessageStarCount(value) => (8, None, None, Some(value)),
        };
        tdmtproto::telegram_api::Function::phone_toggleGroupCallSettings(
            tdmtproto::telegram_api::phone_toggleGroupCallSettings {
                flags,
                reset_invite_hash: matches!(form, ToggleSettingsForm::ResetInviteHash),
                call: Some(group_call.input_group_call()),
                join_muted: join_muted.unwrap_or(false),
                messages_enabled: messages_enabled.unwrap_or(false),
                send_paid_messages_stars: paid_message_star_count.unwrap_or(0),
            },
        )
    }

    /// `get_video_chat_rtmp_stream_url` (cpp:2781-2795) — the
    /// can-manage band resolves at the request arm.
    pub(crate) fn get_video_chat_rtmp_stream_url(
        &mut self,
        env: &mut impl GroupCallEnv,
        dialog_id: DialogId,
        is_story: bool,
        revoke: bool,
        promise: Promise<td_api::RtmpUrl>,
    ) {
        let mut promise = promise;
        if !dialog_id.is_valid() {
            promise.set_error(Status::error(400, "Invalid chat identifier specified"));
            return;
        }
        let Some(peer) = env.get_input_peer_read(dialog_id) else {
            promise.set_error(Status::error(400, "Have no access to the chat"));
            return;
        };
        self.pending.rtmp_url.push(promise);
        self.send_group_call_net_query(
            GroupCallNetKind::GetRtmpUrl { dialog_id },
            tdmtproto::telegram_api::Function::phone_getGroupCallStreamRtmpUrl(
                tdmtproto::telegram_api::phone_getGroupCallStreamRtmpUrl {
                    flags: if is_story { 1 } else { 0 },
                    live_story: is_story,
                    peer: Some(Box::new(peer)),
                    revoke,
                },
            ),
            "phone.getGroupCallStreamRtmpUrl",
        );
    }

    /// `end_group_call_screen_sharing` (cpp:3925-3959).
    pub(crate) fn end_group_call_screen_sharing(
        &mut self,
        group_call_id: GroupCallId,
        promise: Promise<()>,
    ) {
        let mut promise = promise;
        let Some(group_call) = self.resolve_group_call(&mut promise, group_call_id) else {
            return;
        };
        if !group_call.is_active {
            promise.set_error(Status::error(400, "GROUPCALL_JOIN_MISSING"));
            return;
        }
        if !group_call.is_joined {
            promise.set_error(Status::error(400, "GROUPCALL_JOIN_MISSING"));
            return;
        }
        if group_call.is_live_story {
            promise.set_error(Status::error(
                400,
                "Can't use screen sharing in live stories",
            ));
            return;
        }
        self.pending.leave_presentation.push(promise);
        self.send_group_call_net_query(
            GroupCallNetKind::LeavePresentation,
            tdmtproto::telegram_api::Function::phone_leaveGroupCallPresentation(
                tdmtproto::telegram_api::phone_leaveGroupCallPresentation {
                    call: Some(group_call.input_group_call()),
                },
            ),
            "phone.leaveGroupCallPresentation",
        );
    }

    /// `toggle_group_call_is_my_presentation_paused` (cpp:3960-4018) —
    /// the local pending-flag settle (no net).
    pub(crate) fn toggle_group_call_is_my_presentation_paused(
        &mut self,
        group_call_id: GroupCallId,
        is_my_presentation_paused: bool,
        promise: Promise<()>,
    ) {
        let mut promise = promise;
        let Some(group_call) = self.resolve_group_call(&mut promise, group_call_id) else {
            return;
        };
        if !group_call.is_active || !group_call.is_joined {
            promise.set_error(Status::error(400, "GROUPCALL_JOIN_MISSING"));
            return;
        }
        if is_my_presentation_paused == group_call.is_my_video_paused {
            promise.set_value(());
            return;
        }
        if group_call.is_live_story {
            promise.set_error(Status::error(
                400,
                "Can't use screen sharing in live stories",
            ));
            return;
        }
        // there is no reason to save promise; we will send an update
        // with actual value anyway (cpp:4005) — the pending flag intake
        // rides the media plane (§5).
        promise.set_value(());
    }

    /// `toggle_group_call_is_my_video_paused` (cpp:4020-4076) — the
    /// local settle (no net; the pending flag intake rides the media
    /// plane, §5).
    pub(crate) fn toggle_group_call_is_my_video_paused(
        &mut self,
        group_call_id: GroupCallId,
        _is_my_video_paused: bool,
        promise: Promise<()>,
    ) {
        let mut promise = promise;
        let Some(group_call) = self.resolve_group_call(&mut promise, group_call_id) else {
            return;
        };
        if !group_call.is_active || !group_call.is_joined {
            promise.set_error(Status::error(400, "GROUPCALL_JOIN_MISSING"));
            return;
        }
        // there is no reason to save promise; we will send an update
        // with actual value anyway (cpp:4066)
        promise.set_value(());
    }

    /// `toggle_group_call_is_my_video_enabled` (cpp:4077-4094) — the
    /// local settle (no net; the pending flag intake rides the media
    /// plane, §5).
    pub(crate) fn toggle_group_call_is_my_video_enabled(
        &mut self,
        group_call_id: GroupCallId,
        _is_my_video_enabled: bool,
        promise: Promise<()>,
    ) {
        let mut promise = promise;
        let Some(group_call) = self.resolve_group_call(&mut promise, group_call_id) else {
            return;
        };
        if !group_call.is_active || !group_call.is_joined {
            promise.set_error(Status::error(400, "GROUPCALL_JOIN_MISSING"));
            return;
        }
        promise.set_value(());
    }

    /// `toggle_group_call_recording` (cpp:4298-4348).
    pub(crate) fn toggle_group_call_recording(
        &mut self,
        group_call_id: GroupCallId,
        is_enabled: bool,
        title: String,
        record_video: bool,
        use_portrait_orientation: bool,
        promise: Promise<()>,
    ) {
        let mut promise = promise;
        let Some(group_call) = self.resolve_group_call(&mut promise, group_call_id) else {
            return;
        };
        if !group_call.can_be_managed {
            promise.set_error(Status::error(400, "Can't manage group call recording"));
            return;
        }
        if is_enabled == group_call.is_video_recorded {
            promise.set_value(());
            return;
        }
        self.pending.toggle_record.push(promise);
        self.send_group_call_net_query(
            GroupCallNetKind::ToggleRecord,
            tdmtproto::telegram_api::Function::phone_toggleGroupCallRecord(
                tdmtproto::telegram_api::phone_toggleGroupCallRecord {
                    flags: (if is_enabled { 1 } else { 0 })
                        | (if record_video { 4 } else { 0 })
                        | (if !title.is_empty() { 2 } else { 0 }),
                    start: is_enabled,
                    video: record_video,
                    call: Some(group_call.input_group_call()),
                    title,
                    video_portrait: use_portrait_orientation,
                },
            ),
            "phone.toggleGroupCallRecord",
        );
    }

    /// `get_group_call_invite_link` (cpp:4218-4250) — the rights band
    /// (`can_manage_group_call`, cpp:4236) reads the full rights plane;
    /// the port's `can_be_managed` record field stands in.
    pub(crate) fn get_group_call_invite_link(
        &mut self,
        group_call_id: GroupCallId,
        can_self_unmute: bool,
        promise: Promise<String>,
    ) {
        let mut promise = promise;
        let Some(group_call) = self.resolve_group_call_typed(&mut promise, group_call_id) else {
            return;
        };
        if !group_call.is_active {
            promise.set_error(Status::error(400, "Can't get group call invite link"));
            return;
        }
        if !group_call.can_be_managed {
            promise.set_error(Status::error(400, "Not enough rights in the group call"));
            return;
        }
        self.pending.export_invite.push(promise);
        self.send_group_call_net_query(
            GroupCallNetKind::ExportInvite,
            tdmtproto::telegram_api::Function::phone_exportGroupCallInvite(
                tdmtproto::telegram_api::phone_exportGroupCallInvite {
                    flags: if can_self_unmute { 1 } else { 0 },
                    can_self_unmute,
                    call: Some(group_call.input_group_call()),
                },
            ),
            "phone.exportGroupCallInvite",
        );
    }

    /// `invite_group_call_participants` (cpp:3895-3924) — the self
    /// removal + the empty-input settle; the input users resolve at the
    /// request arm.
    pub(crate) fn invite_group_call_participants(
        &mut self,
        group_call_id: GroupCallId,
        input_users: Vec<tdmtproto::telegram_api::InputUser>,
        promise: Promise<()>,
    ) {
        let mut promise = promise;
        let Some(group_call) = self.resolve_group_call(&mut promise, group_call_id) else {
            return;
        };
        if !group_call.is_active {
            promise.set_error(Status::error(400, "Group call is not active"));
            return;
        }
        if group_call.is_conference || group_call.is_live_story {
            promise.set_error(Status::error(400, "The call is not a video chat"));
            return;
        }
        if input_users.is_empty() {
            promise.set_value(());
            return;
        }
        self.pending.invite.push(promise);
        self.send_group_call_net_query(
            GroupCallNetKind::Invite,
            tdmtproto::telegram_api::Function::phone_inviteToGroupCall(
                tdmtproto::telegram_api::phone_inviteToGroupCall {
                    call: Some(group_call.input_group_call()),
                    users: input_users,
                },
            ),
            "phone.inviteToGroupCall",
        );
    }

    /// `delete_group_call_participants` (cpp:3878-3894) — the self band
    /// and the conference band; the conference net leg is the tde2e
    /// `DeleteConferenceCallParticipantsQuery` (deferred plane).
    pub(crate) fn delete_group_call_participants(
        &mut self,
        group_call_id: GroupCallId,
        user_ids: &[i64],
        my_user_id: i64,
        promise: Promise<()>,
    ) {
        let mut promise = promise;
        let Some(group_call) = self.resolve_group_call(&mut promise, group_call_id) else {
            return;
        };
        for &user_id in user_ids {
            if user_id == my_user_id {
                promise.set_error(Status::error(
                    400,
                    "Use leaveGroupCall to leave the group call",
                ));
                return;
            }
        }
        if !group_call.is_conference || group_call.is_live_story {
            promise.set_error(Status::error(
                400,
                "Use setChatMemberStatus or setMessageSenderBlockList to ban participants from video chats or live stories",
            ));
            return;
        }
        // the conference leg rides the tde2e plane — the promise parks
        // per the §5 fidelity notes.
        std::mem::forget(promise);
    }

    /// `decline_group_call_invitation` (cpp:1316-1323) — the invitation
    /// message id resolves at the request arm.
    pub(crate) fn decline_group_call_invitation(
        &mut self,
        server_message_id: i32,
        promise: Promise<()>,
    ) {
        self.pending.decline_invite.push(promise);
        self.send_group_call_net_query(
            GroupCallNetKind::DeclineInvite,
            tdmtproto::telegram_api::Function::phone_declineConferenceCallInvite(
                tdmtproto::telegram_api::phone_declineConferenceCallInvite {
                    msg_id: server_message_id,
                },
            ),
            "phone.declineConferenceCallInvite",
        );
    }

    /// `send_group_call_message` (cpp:5884-5979) — the star/text bands
    /// at the port depth; the message-store intake rides the media
    /// plane (§5).
    pub(crate) fn send_group_call_message(
        &mut self,
        group_call_id: GroupCallId,
        paid_message_star_count: i64,
        is_reaction: bool,
        is_my_dialog: bool,
        has_owned_star_count: bool,
        promise: Promise<()>,
    ) {
        let mut promise = promise;
        let Some(group_call) = self.resolve_group_call(&mut promise, group_call_id) else {
            return;
        };
        if paid_message_star_count < 0 || (is_reaction && paid_message_star_count == 0) {
            promise.set_error(Status::error(
                400,
                "Invalid number of Telegram Stars specified",
            ));
            return;
        }
        if !group_call.is_joined {
            promise.set_error(Status::error(400, "GROUPCALL_JOIN_MISSING"));
            return;
        }
        if group_call.is_live_story {
            if paid_message_star_count > 0 && is_my_dialog {
                promise.set_error(Status::error(400, "Can't send paid messages to self"));
                return;
            }
            if !is_reaction && !has_owned_star_count {
                promise.set_error(Status::error(400, "Have not enough Telegram Stars"));
                return;
            }
        } else if paid_message_star_count != 0 {
            if is_reaction {
                promise.set_error(Status::error(400, "Reactions can't be sent to the call"));
            } else {
                promise.set_error(Status::error(
                    400,
                    "Paid messages can't be sent to the call",
                ));
            }
            return;
        }
        self.pending.send_message.push(promise);
        self.send_group_call_net_query(
            GroupCallNetKind::SendMessage {
                paid_message_star_count,
            },
            tdmtproto::telegram_api::Function::phone_sendGroupCallMessage(
                tdmtproto::telegram_api::phone_sendGroupCallMessage {
                    flags: if paid_message_star_count > 0 { 1 } else { 0 },
                    call: Some(group_call.input_group_call()),
                    random_id: 0,
                    message: Some(tdmtproto::telegram_api::TextWithEntities {
                        text: String::new(),
                        entities: Vec::new(),
                    }),
                    allow_paid_stars: paid_message_star_count.max(0),
                    send_as: None,
                },
            ),
            "phone.sendGroupCallMessage",
        );
    }

    /// `send_group_call_reaction` (cpp:5980-6045) — the star bands +
    /// the pending bookkeeping (the store rides the media plane, §5;
    /// the promise settles locally like the C++).
    pub(crate) fn send_group_call_reaction(
        &mut self,
        group_call_id: GroupCallId,
        star_count: i64,
        is_my_dialog: bool,
        has_owned_star_count: bool,
        promise: Promise<()>,
    ) {
        let mut promise = promise;
        let Some(group_call) = self.resolve_group_call(&mut promise, group_call_id) else {
            return;
        };
        if star_count <= 0 || star_count > 1_000_000_000 {
            promise.set_error(Status::error(
                400,
                "Invalid number of Telegram Stars specified",
            ));
            return;
        }
        if !group_call.is_joined {
            promise.set_error(Status::error(400, "GROUPCALL_JOIN_MISSING"));
            return;
        }
        if !group_call.is_live_story {
            promise.set_error(Status::error(400, "Reactions can't be sent to the call"));
            return;
        }
        if is_my_dialog {
            promise.set_error(Status::error(400, "Can't send paid reactions to self"));
            return;
        }
        if !has_owned_star_count {
            promise.set_error(Status::error(400, "Have not enough Telegram Stars"));
            return;
        }
        if group_call.paid_message_star_count > 1_000_000_000 {
            promise.set_error(Status::error(400, "Too many Stars added"));
            return;
        }
        promise.set_value(());
    }

    /// `delete_group_call_messages` (cpp:4703-4760) — the message store
    /// rides the media plane (§5): the per-message bands pass (no
    /// stored messages) and the empty server-id list settles locally.
    pub(crate) fn delete_group_call_messages(
        &mut self,
        group_call_id: GroupCallId,
        promise: Promise<()>,
    ) {
        let mut promise = promise;
        let Some(group_call) = self.resolve_group_call(&mut promise, group_call_id) else {
            return;
        };
        if !group_call.is_joined {
            promise.set_error(Status::error(400, "GROUPCALL_JOIN_MISSING"));
            return;
        }
        // the stored-call-message delete + the server_ids list ride the
        // media plane; the port has no stored messages to delete.
        promise.set_value(());
    }

    /// `delete_group_call_messages_by_sender` (cpp:4761-4800) — the
    /// sender form dispatches without the message store.
    pub(crate) fn delete_group_call_messages_by_sender(
        &mut self,
        env: &mut impl GroupCallEnv,
        group_call_id: GroupCallId,
        sender_dialog_id: DialogId,
        report_spam: bool,
        promise: Promise<()>,
    ) {
        let mut promise = promise;
        let Some(group_call) = self.resolve_group_call(&mut promise, group_call_id) else {
            return;
        };
        if !group_call.is_joined {
            promise.set_error(Status::error(400, "GROUPCALL_JOIN_MISSING"));
            return;
        }
        // `get_group_call_can_delete_messages` (cpp:3069-3074).
        if !(group_call.is_live_story && group_call.can_be_managed) {
            promise.set_error(Status::error(
                400,
                "Can't delete messages in the group call",
            ));
            return;
        }
        let Some(peer) = env.get_input_peer_read(sender_dialog_id) else {
            promise.set_error(Status::error(400, "Message sender not found"));
            return;
        };
        self.pending.delete_sender_messages.push(promise);
        self.send_group_call_net_query(
            GroupCallNetKind::DeleteSenderMessages { sender_dialog_id },
            tdmtproto::telegram_api::Function::phone_deleteGroupCallParticipantMessages(
                tdmtproto::telegram_api::phone_deleteGroupCallParticipantMessages {
                    flags: if report_spam { 1 } else { 0 },
                    report_spam,
                    call: Some(group_call.input_group_call()),
                    participant: Some(Box::new(peer)),
                },
            ),
            "phone.deleteGroupCallParticipantMessages",
        );
    }

    /// `load_group_call_participants` (cpp:5060-5088).
    pub(crate) fn load_group_call_participants(
        &mut self,
        group_call_id: GroupCallId,
        limit: i32,
        promise: Promise<()>,
    ) {
        let mut promise = promise;
        if limit <= 0 {
            promise.set_error(Status::error(400, "Parameter limit must be positive"));
            return;
        }
        let Some(group_call) = self.resolve_group_call(&mut promise, group_call_id) else {
            return;
        };
        if group_call.is_live_story {
            promise.set_error(Status::error(400, "Can't load group call participants"));
            return;
        }
        self.pending.get_participants.push(promise);
        self.send_group_call_net_query(
            GroupCallNetKind::GetParticipants,
            tdmtproto::telegram_api::Function::phone_getGroupParticipants(
                tdmtproto::telegram_api::phone_getGroupParticipants {
                    call: Some(group_call.input_group_call()),
                    ids: Vec::new(),
                    sources: Vec::new(),
                    offset: String::new(),
                    limit,
                },
            ),
            "phone.getGroupParticipants",
        );
    }

    /// `get_group_call_stars_from_server` (cpp:4576-4590) — the donors
    /// leg; the local-loaded band rides the participants plane (§5).
    pub(crate) fn get_group_call_stars_from_server(
        &mut self,
        group_call_id: GroupCallId,
        promise: Promise<td_api::LiveStoryDonors>,
    ) {
        let mut promise = promise;
        let Some(group_call) = self.resolve_group_call_typed(&mut promise, group_call_id) else {
            return;
        };
        if !group_call.is_joined {
            promise.set_error(Status::error(400, "GROUPCALL_JOIN_MISSING"));
            return;
        }
        if !group_call.is_live_story {
            promise.set_error(Status::error(400, "The group call isn't a live story"));
            return;
        }
        self.pending.get_stars.push(promise);
        self.send_group_call_net_query(
            GroupCallNetKind::GetStars,
            tdmtproto::telegram_api::Function::phone_getGroupCallStars(
                tdmtproto::telegram_api::phone_getGroupCallStars {
                    call: Some(group_call.input_group_call()),
                },
            ),
            "phone.getGroupCallStars",
        );
    }

    /// `get_group_call_streams` (cpp:4246-4293).
    pub(crate) fn get_group_call_streams(
        &mut self,
        group_call_id: GroupCallId,
        promise: Promise<td_api::GroupCallStreams>,
    ) {
        let mut promise = promise;
        let Some(group_call) = self.resolve_group_call_typed(&mut promise, group_call_id) else {
            return;
        };
        if group_call.is_conference || !group_call.is_active || group_call.stream_dc_id == 0 {
            promise.set_error(Status::error(400, "Group call can't be streamed"));
            return;
        }
        if !group_call.is_joined {
            promise.set_error(Status::error(400, "GROUPCALL_JOIN_MISSING"));
            return;
        }
        self.pending.get_streams.push(promise);
        self.send_group_call_net_query(
            GroupCallNetKind::GetStreams,
            tdmtproto::telegram_api::Function::phone_getGroupCallStreamChannels(
                tdmtproto::telegram_api::phone_getGroupCallStreamChannels {
                    call: Some(group_call.input_group_call()),
                },
            ),
            "phone.getGroupCallStreamChannels",
        );
    }

    /// `leave_group_call` (cpp:5090-5127) — the media-plane join bands
    /// collapse onto the joined band at the port depth.
    pub(crate) fn leave_group_call(&mut self, group_call_id: GroupCallId, promise: Promise<()>) {
        let mut promise = promise;
        let Some(group_call) = self.resolve_group_call(&mut promise, group_call_id) else {
            return;
        };
        if !group_call.is_joined {
            promise.set_error(Status::error(400, "GROUPCALL_JOIN_MISSING"));
            return;
        }
        self.pending.leave.push(promise);
        self.send_group_call_net_query(
            GroupCallNetKind::Leave,
            tdmtproto::telegram_api::Function::phone_leaveGroupCall(
                tdmtproto::telegram_api::phone_leaveGroupCall {
                    call: Some(group_call.input_group_call()),
                    source: group_call.audio_source,
                },
            ),
            "phone.leaveGroupCall",
        );
    }

    /// `commit_pending_group_call_reactions` (cpp:6046-6108) — the
    /// aggregated send rides the pending-reaction store (the media
    /// plane, §5); the bands settle locally like the C++.
    pub(crate) fn commit_pending_group_call_reactions(
        &mut self,
        group_call_id: GroupCallId,
        promise: Promise<()>,
    ) {
        let mut promise = promise;
        let Some(group_call) = self.resolve_group_call(&mut promise, group_call_id) else {
            return;
        };
        if !group_call.is_joined {
            promise.set_error(Status::error(400, "GROUPCALL_JOIN_MISSING"));
            return;
        }
        if !group_call.is_live_story {
            promise.set_error(Status::error(400, "Reactions can't be sent to the call"));
            return;
        }
        // the pending-reaction store + the aggregated
        // SendGroupCallMessageQuery ride the media plane.
        promise.set_value(());
    }

    /// `remove_pending_group_call_reactions` (cpp:6109-6142) — the
    /// bands + the unconditional settle (the store rides the media
    /// plane, §5).
    pub(crate) fn remove_pending_group_call_reactions(
        &mut self,
        group_call_id: GroupCallId,
        promise: Promise<()>,
    ) {
        let mut promise = promise;
        let Some(group_call) = self.resolve_group_call(&mut promise, group_call_id) else {
            return;
        };
        if !group_call.is_joined {
            promise.set_error(Status::error(400, "GROUPCALL_JOIN_MISSING"));
            return;
        }
        if !group_call.is_live_story {
            promise.set_error(Status::error(400, "Reactions can't be sent to the call"));
            return;
        }
        promise.set_value(());
    }

    /// `invite_group_call_participant` (cpp:3796-3876) — the
    /// conference-plane bands; the InviteConferenceCallParticipant
    /// answer's success leg rides the messages plane
    /// (`wait_message_add`, §5) — the dispatch parks.
    pub(crate) fn invite_group_call_participant(
        &mut self,
        group_call_id: GroupCallId,
        _input_user: tdmtproto::telegram_api::InputUser,
        _is_video: bool,
        promise: Promise<td_api::InviteGroupCallParticipantResult>,
    ) {
        let mut promise = promise;
        let Some(group_call) = self.resolve_group_call_typed(&mut promise, group_call_id) else {
            return;
        };
        if !group_call.is_conference || group_call.is_live_story {
            promise.set_error(Status::error(
                400,
                "Use inviteVideoChatParticipants for video chats",
            ));
            return;
        }
        if !group_call.is_active || !group_call.is_joined {
            promise.set_error(Status::error(400, "GROUPCALL_JOIN_MISSING"));
            return;
        }
        self.park_media_query("phone.inviteConferenceCallParticipant", promise);
    }

    /// `discard_group_call` (cpp:7255-7259) — the id gate + the send.
    pub(crate) fn discard_group_call(&mut self, group_call_id: GroupCallId, promise: Promise<()>) {
        let mut promise = promise;
        let Some(group_call) = self.resolve_group_call(&mut promise, group_call_id) else {
            return;
        };
        self.pending.discard.push(promise);
        self.send_group_call_net_query(
            GroupCallNetKind::Discard,
            tdmtproto::telegram_api::Function::phone_discardGroupCall(
                tdmtproto::telegram_api::phone_discardGroupCall {
                    call: Some(group_call.input_group_call()),
                },
            ),
            "phone.discardGroupCall",
        );
    }

    /// The deferred-plane park — the join/payload/participant flows
    /// (createVideoChat, createGroupCall, joinGroupCall, joinVideoChat,
    /// joinLiveStory, startGroupCallScreenSharing, the participant
    /// trio) ride the media plane (ticket td-rs/td#54); the promise
    /// parks per the §5 fidelity notes.
    pub(crate) fn park_media_query<T: Send + 'static>(&mut self, what: &str, promise: Promise<T>) {
        tdutils::log_info!("Send {what} — [net-inert]");
        std::mem::forget(promise);
    }

    /// `send_update_group_call` (cpp:2927-2952) — the port's thin form:
    /// the update carries the placeholder object (the full
    /// `get_group_call_object` emission lands with the media plane).
    fn send_update_group_call(&self, env: &mut impl GroupCallEnv, group_call: &GroupCall) {
        let _ = group_call;
        env.send_update(td_api::Update::updateGroupCall(td_api::updateGroupCall {
            group_call: None,
        }));
    }

    /// `get_current_state` (Requests.cpp:2253).
    pub(crate) fn get_current_state(
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
    use std::sync::Arc;
    use std::sync::Mutex;

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

    fn test_call(id: i32) -> GroupCall {
        GroupCall {
            group_call_id: GroupCallId::new(id),
            input_group_call_id: 1000 + id as i64,
            input_group_call_access_hash: 42,
            chat_id: -1_000_000_000_000 - id as i64,
            title: "Call".to_owned(),
            is_active: true,
            is_joined: true,
            ..GroupCall::default()
        }
    }

    /// The unit-promise slot — captures the settled result.
    /// The settled-result slot type.
    type ResultSlot = Arc<Mutex<Option<tdutils::status::Result<()>>>>;

    fn unit_slot() -> (Promise<()>, ResultSlot) {
        let slot: ResultSlot = Arc::new(Mutex::new(None));
        let capture = Arc::clone(&slot);
        let promise = tdutils::promise::PromiseCreator::lambda_result(move |result| {
            *capture.lock().unwrap() = Some(result);
        });
        (promise, slot)
    }

    fn slot_error(slot: &ResultSlot) -> Option<(i32, String)> {
        slot.lock().unwrap().as_ref().map(|r| match r {
            tdutils::status::Result::Ok(()) => (0, String::new()),
            tdutils::status::Result::Err(status) => (status.code(), status.message().to_owned()),
        })
    }

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
        assert_eq!(
            GroupCallManager::check_group_call_id(GroupCallId::default()).message(),
            "Invalid group call identifier specified"
        );
    }

    #[test]
    fn intake_registers_the_call() {
        let mut manager = GroupCallManager::new();
        let mut env = TestEnv::default();
        manager.on_get_group_call(&mut env, test_call(5));
        assert!(manager.get_group_call(GroupCallId::new(5)).is_some());
    }

    /// `get_input_group_call_id` (cpp:2283-2292) — the invalid and the
    /// unregistered bands.
    #[test]
    fn resolve_rejects_invalid_and_unregistered_ids() {
        let mut manager = GroupCallManager::new();
        let (promise, slot) = unit_slot();
        manager.discard_group_call(GroupCallId::default(), promise);
        assert_eq!(
            slot_error(&slot),
            Some((400, "Invalid group call identifier specified".to_owned()))
        );

        let (promise, slot) = unit_slot();
        manager.discard_group_call(GroupCallId::new(7), promise);
        assert_eq!(
            slot_error(&slot),
            Some((400, "Wrong group call identifier specified".to_owned()))
        );
    }

    /// `start_scheduled_group_call` (cpp:4400-4432) — the band ladder
    /// and the net-inert dispatch.
    #[test]
    fn start_scheduled_bands() {
        let mut env = TestEnv::default();

        // the not-scheduled band (conference)
        let mut manager = GroupCallManager::new();
        let mut call = test_call(1);
        call.is_conference = true;
        manager.on_get_group_call(&mut env, call);
        let (promise, slot) = unit_slot();
        manager.start_scheduled_group_call(GroupCallId::new(1), promise);
        assert_eq!(
            slot_error(&slot),
            Some((400, "The group call isn't scheduled".to_owned()))
        );

        // the rights band
        let mut manager = GroupCallManager::new();
        let mut call = test_call(1);
        call.can_be_managed = false;
        manager.on_get_group_call(&mut env, call);
        let (promise, slot) = unit_slot();
        manager.start_scheduled_group_call(GroupCallId::new(1), promise);
        assert_eq!(
            slot_error(&slot),
            Some((400, "Not enough rights to start the group call".to_owned()))
        );

        // the already-ended band
        let mut manager = GroupCallManager::new();
        let mut call = test_call(1);
        call.can_be_managed = true;
        call.is_active = false;
        manager.on_get_group_call(&mut env, call);
        let (promise, slot) = unit_slot();
        manager.start_scheduled_group_call(GroupCallId::new(1), promise);
        assert_eq!(
            slot_error(&slot),
            Some((400, "Group call already ended".to_owned()))
        );

        // the unscheduled local settle (managed + active + date 0)
        let mut manager = GroupCallManager::new();
        let mut call = test_call(1);
        call.can_be_managed = true;
        manager.on_get_group_call(&mut env, call);
        let (promise, slot) = unit_slot();
        manager.start_scheduled_group_call(GroupCallId::new(1), promise);
        assert_eq!(slot_error(&slot), Some((0, String::new())));

        // the scheduled dispatch (net-inert: the send logs; the promise
        // parks in the lot)
        let mut manager = GroupCallManager::new();
        let mut call = test_call(1);
        call.can_be_managed = true;
        call.scheduled_start_date = 1700000000;
        manager.on_get_group_call(&mut env, call);
        let (promise, slot) = unit_slot();
        manager.start_scheduled_group_call(GroupCallId::new(1), promise);
        assert_eq!(slot_error(&slot), None);
        assert_eq!(manager.pending.start_scheduled.len(), 1);
    }

    /// `set_group_call_title` (cpp:5193-5236) — the manage band, the
    /// equality settle and the update emission.
    #[test]
    fn set_title_bands_and_settle() {
        let mut env = TestEnv::default();

        let mut manager = GroupCallManager::new();
        manager.on_get_group_call(&mut env, test_call(3));
        let (promise, slot) = unit_slot();
        manager.set_group_call_title(&mut env, GroupCallId::new(3), "next".to_owned(), promise);
        assert_eq!(
            slot_error(&slot),
            Some((400, "Can't change group call title".to_owned()))
        );

        let mut call = test_call(3);
        call.can_be_managed = true;
        call.title = "same".to_owned();
        let mut manager = GroupCallManager::new();
        manager.on_get_group_call(&mut env, call.clone());
        env.updates.clear();
        let (promise, slot) = unit_slot();
        manager.set_group_call_title(&mut env, GroupCallId::new(3), "same".to_owned(), promise);
        assert_eq!(slot_error(&slot), Some((0, String::new())));
        assert!(env.updates.is_empty());

        // the change settles locally, emits the update and parks the
        // query-side promise (the net-inert send logs)
        let (promise, slot) = unit_slot();
        manager.set_group_call_title(&mut env, GroupCallId::new(3), "next".to_owned(), promise);
        assert_eq!(slot_error(&slot), Some((0, String::new())));
        assert_eq!(env.updates.len(), 1);
        assert_eq!(manager.pending.edit_title.len(), 1);
        assert_eq!(
            manager.get_group_call(GroupCallId::new(3)).unwrap().title,
            "next"
        );
        let _ = call;
    }

    /// `toggle_group_call_mute_new_participants` (cpp:5617-5655) — the
    /// equality settle + the update emission + the net-inert dispatch.
    #[test]
    fn toggle_mute_new_participants() {
        let mut env = TestEnv::default();
        let mut call = test_call(4);
        call.mute_new_participants = false;
        let mut manager = GroupCallManager::new();
        manager.on_get_group_call(&mut env, call);
        env.updates.clear();

        // the equality settle
        let (promise, slot) = unit_slot();
        manager.toggle_group_call_mute_new_participants(
            &mut env,
            GroupCallId::new(4),
            false,
            promise,
        );
        assert_eq!(slot_error(&slot), Some((0, String::new())));
        assert!(env.updates.is_empty());

        // the change dispatch
        let (promise, slot) = unit_slot();
        manager.toggle_group_call_mute_new_participants(
            &mut env,
            GroupCallId::new(4),
            true,
            promise,
        );
        assert_eq!(slot_error(&slot), Some((0, String::new())));
        assert_eq!(env.updates.len(), 1);
        assert_eq!(manager.pending.toggle_settings.len(), 1);
        assert!(
            manager
                .get_group_call(GroupCallId::new(4))
                .unwrap()
                .mute_new_participants
        );
    }

    /// `revoke_group_call_invite_link` (cpp:6429-6431) — the manage
    /// band and the net-inert dispatch.
    #[test]
    fn revoke_invite_link_bands() {
        let mut env = TestEnv::default();
        let mut manager = GroupCallManager::new();
        manager.on_get_group_call(&mut env, test_call(6));
        let (promise, slot) = unit_slot();
        manager.revoke_group_call_invite_link(GroupCallId::new(6), promise);
        assert_eq!(
            slot_error(&slot),
            Some((400, "Can't revoke invite link in the group call".to_owned()))
        );

        let mut call = test_call(6);
        call.can_be_managed = true;
        let mut manager = GroupCallManager::new();
        manager.on_get_group_call(&mut env, call);
        let (promise, slot) = unit_slot();
        manager.revoke_group_call_invite_link(GroupCallId::new(6), promise);
        assert_eq!(slot_error(&slot), None);
        assert_eq!(manager.pending.toggle_settings.len(), 1);
    }

    /// `send_group_call_reaction` (cpp:5980-6045) — the star band
    /// ladder settles locally.
    #[test]
    fn reaction_bands() {
        let mut env = TestEnv::default();
        let mut call = test_call(7);
        call.is_live_story = true;
        call.chat_id = -1_000_000_000_000 - 7;
        let mut manager = GroupCallManager::new();
        manager.on_get_group_call(&mut env, call);

        // the invalid star count
        let (promise, slot) = unit_slot();
        manager.send_group_call_reaction(GroupCallId::new(7), 0, false, true, promise);
        assert_eq!(
            slot_error(&slot),
            Some((400, "Invalid number of Telegram Stars specified".to_owned()))
        );

        // the not-a-live-story band over the plain call
        let mut manager = GroupCallManager::new();
        manager.on_get_group_call(&mut env, test_call(7));
        let (promise, slot) = unit_slot();
        manager.send_group_call_reaction(GroupCallId::new(7), 5, false, true, promise);
        assert_eq!(
            slot_error(&slot),
            Some((400, "Reactions can't be sent to the call".to_owned()))
        );

        // the happy path settles locally
        let mut call = test_call(7);
        call.is_live_story = true;
        let mut manager = GroupCallManager::new();
        manager.on_get_group_call(&mut env, call);
        let (promise, slot) = unit_slot();
        manager.send_group_call_reaction(GroupCallId::new(7), 5, false, true, promise);
        assert_eq!(slot_error(&slot), Some((0, String::new())));
    }

    /// `discard_group_call` (cpp:7255-7259) — the id gate + the
    /// net-inert dispatch.
    #[test]
    fn discard_dispatches() {
        let mut env = TestEnv::default();
        let mut manager = GroupCallManager::new();
        manager.on_get_group_call(&mut env, test_call(8));
        let (promise, slot) = unit_slot();
        manager.discard_group_call(GroupCallId::new(8), promise);
        assert_eq!(slot_error(&slot), None);
        assert_eq!(manager.pending.discard.len(), 1);
    }

    /// `load_group_call_participants` (cpp:5060-5088) — the limit band
    /// and the live-story band.
    #[test]
    fn load_participants_bands() {
        let mut env = TestEnv::default();
        let mut manager = GroupCallManager::new();
        manager.on_get_group_call(&mut env, test_call(9));

        let (promise, slot) = unit_slot();
        manager.load_group_call_participants(GroupCallId::new(9), 0, promise);
        assert_eq!(
            slot_error(&slot),
            Some((400, "Parameter limit must be positive".to_owned()))
        );

        let mut call = test_call(9);
        call.is_live_story = true;
        let mut manager = GroupCallManager::new();
        manager.on_get_group_call(&mut env, call);
        let (promise, slot) = unit_slot();
        manager.load_group_call_participants(GroupCallId::new(9), 10, promise);
        assert_eq!(
            slot_error(&slot),
            Some((400, "Can't load group call participants".to_owned()))
        );
    }

    /// The local settles keep the net-inert posture (no promise leak,
    /// no dispatch).
    #[test]
    fn local_settles() {
        let mut env = TestEnv::default();
        let mut manager = GroupCallManager::new();
        manager.on_get_group_call(&mut env, test_call(10));

        let (promise, slot) = unit_slot();
        manager.toggle_group_call_is_my_video_paused(GroupCallId::new(10), true, promise);
        assert_eq!(slot_error(&slot), Some((0, String::new())));

        let (promise, slot) = unit_slot();
        manager.toggle_group_call_is_my_video_enabled(GroupCallId::new(10), true, promise);
        assert_eq!(slot_error(&slot), Some((0, String::new())));

        let (promise, slot) = unit_slot();
        manager.delete_group_call_messages(GroupCallId::new(10), promise);
        assert_eq!(slot_error(&slot), Some((0, String::new())));
    }

    /// The park keeps the net-inert posture (the promise parks).
    #[test]
    fn media_parks_stay_net_inert() {
        let mut manager = GroupCallManager::new();
        manager.park_media_query::<()>("phone.joinGroupCall", Promise::default());
        manager.park_media_query::<()>("phone.createGroupCall", Promise::default());
    }
}
