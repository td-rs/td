//! `CommunityManager` (td/telegram/CommunityManager.{h,cpp}) — the M.34
//! Wave-3 community manager: the community cache plane and the two request
//! flows (createCommunity/setCommunityName — the only community td_api
//! arms in this revision). Recorded collapses: the Community/CommunityFull
//! byte-exact store/parse + the sqlite-pmc persistence join when the
//! Photo/DialogPhoto store/parse land on master (they live on the unmerged
//! M.9/M.10 lines); the photo object legs answer `None`; the
//! create/rename/permissions/delete queries ride the client-side
//! dispatcher under their `CommunityNetKind` (the answer routing lives in
//! `Td::on_community_net_answer`; the create answer chains behind
//! `on_get_updates` into `finish_create_community`); the full
//! dialog-access plane is the env seam; the dispatch-dependency argument
//! of the C++ `net_query_creator().create` calls collapses (the port's
//! NetQuery carries no dependency set); `get_community_id_object`
//! (CommunityManager.cpp:2174-2181) returns the bare id without the C++
//! unknown-community band (the `unknown_communities_` one-time
//! `updateUnknownCommunity` emission + LOG(ERROR)) — every current
//! caller pre-filters by `have_community`, so the band is unreachable
//! through the real wiring (recorded divergence; #129).

use crate::chat_manager::{AdministratorRights, DialogParticipantStatus, RestrictedRights};
use crate::community_id::CommunityId;
use crate::dialog_id::DialogId;
use crate::messages_manager::{
    DialogNotificationSettings, get_chat_notification_settings_object,
    get_dialog_notification_settings_from_td_api, get_dialog_notification_settings_from_telegram,
    need_update_dialog_notification_settings,
};
use crate::td_api;
use tdmtproto::telegram_api;
use tdutils::promise::Promise;
use tdutils::status::Status;

/// The `G()`/`td_` surface of the manager.
pub trait CommunityManagerEnv {
    fn close_flag(&self) -> bool;

    fn send_update(&mut self, update: td_api::Update);

    /// `DialogManager::check_dialog_access` — the create gate.
    fn check_dialog_access(&mut self, dialog_id: DialogId) -> std::result::Result<(), Status>;

    /// `DialogManager::get_chat_id_object` — the collapsed sync form.
    fn get_chat_id_object(&mut self, dialog_id: DialogId) -> i64;

    /// `DialogFilterManager::remove_community_from_dialog_filters`
    /// (ea2241d48) — the collapse-change fan-out.
    fn remove_community_from_dialog_filters(&mut self, community_id: CommunityId) {
        tdutils::log_info!(
            "Remove community {community_id:?} from dialog filters — [dialog-filter seam]"
        );
    }

    /// `CreateCommunityQuery::send` (CommunityManager.cpp:129-136) —
    /// `communities.create` over the read-access input peer of the
    /// dialog; the answer routes through `on_get_updates` with the
    /// `finish_create_community` chain. The dispatch-dependency argument
    /// of the C++ `net_query_creator().create` collapses (the port's
    /// NetQuery carries no dependency set).
    fn send_create_community_query(&mut self, name: &str, dialog_id: DialogId, is_hidden: bool) {
        tdutils::log_info!(
            "Send communities.create(name = {name}, is_hidden = {is_hidden}) for {dialog_id:?} — [net-inert]"
        );
        let _ = dialog_id;
    }

    /// `GetFullCommunityQuery::send` — the community-full cache load
    /// (no ported flow drives it yet; the seam stays for the
    /// get-community plane).
    fn send_get_full_community_query(&mut self, community_id: CommunityId) {
        tdutils::log_info!("Send channels.getFullCommunity for {community_id:?} — [net-inert]");
    }

    /// `EditCommunityTitleQuery::send` (CommunityManager.cpp:163-194) —
    /// `channels.editTitle` over `get_input_community`; the resolved
    /// input channel rides the seam (the manager is taken out under the
    /// env).
    fn send_set_community_name_query(
        &mut self,
        input_community: telegram_api::InputChannel,
        name: &str,
    ) {
        tdutils::log_info!("Send channels.editTitle to {name:?} — [net-inert]");
        let _ = input_community;
    }

    /// Whether the AuthorizationManager reports a bot — the
    /// `get_input_peer`/`get_input_community` bots-tolerant arm
    /// (CommunityManager.cpp:1375-1398).
    fn auth_is_bot(&self) -> bool {
        false
    }

    /// `EditCommunityDefaultBannedRightsQuery::send` (d55f0a694,
    /// CommunityManager.cpp:197-229) — `messages_editChatDefaultBannedRights`
    /// over `get_input_peer` (the resolved peer rides the seam).
    fn send_set_community_permissions_query(
        &mut self,
        input_peer: telegram_api::InputPeer,
        permissions: &RestrictedRights,
    ) {
        tdutils::log_info!(
            "Send messages.editChatDefaultBannedRights, rights = {:?} — [net-inert]",
            permissions.get_chat_banned_rights()
        );
        let _ = input_peer;
    }

    /// `DeleteCommunityQuery::send` (f5f7fe5fd, CommunityManager.cpp:231-260)
    /// — `channels.deleteChannel` over `get_input_community`.
    fn send_delete_community_query(&mut self, input_community: telegram_api::InputChannel) {
        tdutils::log_info!("Send channels.deleteChannel — [net-inert]");
        let _ = input_community;
    }

    /// `G()->unix_time()` — the community notification-settings clock
    /// reads (the 99302f0e2/#113 slice).
    fn unix_time(&self) -> i32 {
        0
    }

    /// The process-monotonic clock the unmute deadlines ride (the C++
    /// `MultiTimeout::set_timeout_in` base; mirrors the
    /// notification-settings manager's `now`).
    fn now(&self) -> f64 {
        f64::from(self.unix_time())
    }

    /// `GetCommunityNotifySettingsQuery::send` (99302f0e2,
    /// CommunityManager.cpp:121-138) — `account_getNotifySettings` over
    /// `inputNotifyCommunity`; the answer routes through the community
    /// net-answer drain (`GetNotifySettings`). The input is resolved
    /// manager-side (the manager is taken out under the env).
    fn send_get_community_notify_settings_query(
        &mut self,
        community_id: CommunityId,
        _input_community: telegram_api::InputChannel,
    ) {
        tdutils::log_info!("Send account_getNotifySettings for {community_id:?} — [net-inert]");
    }

    /// `UpdateCommunityNotifySettingsQuery::send` (99302f0e2,
    /// CommunityManager.cpp:140-168) — `account_updateNotifySettings` over
    /// the full's converted settings; the answer routes through the
    /// community net-answer drain (`UpdateNotifySettings`). The input is
    /// resolved manager-side.
    fn send_update_community_notify_settings_query(
        &mut self,
        community_id: CommunityId,
        input_community: telegram_api::InputChannel,
        notification_settings: &DialogNotificationSettings,
    ) {
        tdutils::log_info!("Send account_updateNotifySettings for {community_id:?} — [net-inert]");
        let _ = (input_community, notification_settings);
    }

    /// `reload_community_full` (c9ed2c390's postpone arm) — the
    /// `channels.getFullCommunity` reload behind the parked settings
    /// (the input resolved manager-side).
    fn reload_community_full(
        &mut self,
        community_id: CommunityId,
        _input_community: telegram_api::InputChannel,
    ) {
        tdutils::log_info!("Reload community full {community_id:?} — [net-inert]");
    }

    /// `GetCommunityPeerLinkRequestsQuery::send` (ffc06ec75,
    /// CommunityManager.cpp:388-410) — `communities_getPeerLinkRequests`
    /// over `get_input_community`; the answer routes through the
    /// community net-answer drain and the manager's page builder
    /// (`on_get_community_peer_link_requests`). The input is resolved
    /// manager-side.
    fn send_get_community_peer_link_requests_query(
        &mut self,
        community_id: CommunityId,
        _input_community: telegram_api::InputChannel,
        _offset: &str,
        _limit: i32,
    ) {
        tdutils::log_info!(
            "Send communities_getPeerLinkRequests for {community_id:?} — [net-inert]"
        );
    }

    /// `UserManager::on_get_users` (CommunityManager.cpp:411 is the first
    /// community-family caller) — the shared wire-users intake: the
    /// requesting users must exist before the page resolves.
    fn on_get_users(&mut self, _users: Vec<telegram_api::User>, _source: &str) {}

    /// `ChatManager::on_get_chats` (CommunityManager.cpp:1867 is the
    /// first community-family caller) — the shared wire-chats intake:
    /// the community dialogs must exist before an answer resolves. The
    /// seam routes through the shared intake (`Td::intake_chats`),
    /// which applies the community-first hop (#143); a manager method
    /// driven under the take-dance (the dispatcher holds the community
    /// manager out of `Td`) still pre-routes the community family
    /// itself — see `on_get_joined_communities` — and hands this seam
    /// the non-community remainder only.
    fn on_get_chats(&mut self, _chats: Vec<telegram_api::Chat>, _source: &str) {}

    /// `DialogManager::force_create_dialog` — the documented no-op stub;
    /// the seam keeps the C++ call sites (the forced dialog creation
    /// collapses until the messages plane lands).
    fn force_create_dialog(&mut self, _dialog_id: DialogId, _source: &str) {}

    /// `UserManager::get_user_id_object` (UserManager.cpp:10278-10291) —
    /// the collapsed sync form.
    fn get_user_id_object(&mut self, user_id: crate::user_manager::UserId, _source: &str) -> i64 {
        user_id.get()
    }

    /// `ProcessCommunityPeerLinkRequestQuery::send` (2b80bf533,
    /// CommunityManager.cpp:442-475) —
    /// `communities_togglePeerLinkRequestApproval` with
    /// `reject = !approve`; the requested chat's read-access input peer
    /// resolves at the seam (the manager is taken out under the env).
    /// The answer's `Bool` is ignored — the request settles through the
    /// change-settle.
    fn send_process_community_peer_link_request_query(
        &mut self,
        community_id: CommunityId,
        _dialog_id: DialogId,
        _input_community: telegram_api::InputChannel,
        _approve: bool,
    ) {
        tdutils::log_info!(
            "Send communities_togglePeerLinkRequestApproval for {community_id:?} — [net-inert]"
        );
    }

    /// `ProcessCommunityPeerLinkRequestsQuery::send` (79a943019,
    /// CommunityManager.cpp:477-505) —
    /// `communities_toggleAllPeerLinkRequestApproval` with
    /// `reject = !approve`; the `Bool` answer is ignored — the request
    /// settles through the change-settle.
    fn send_process_community_peer_link_requests_query(
        &mut self,
        community_id: CommunityId,
        _input_community: telegram_api::InputChannel,
        _approve: bool,
    ) {
        tdutils::log_info!(
            "Send communities_toggleAllPeerLinkRequestApproval for {community_id:?} — [net-inert]"
        );
    }

    /// The `MessagesManager::on_update_dialog_community_notification_settings`
    /// fan-out (0063421d6) — seam until the messages notification plane;
    /// the C++ body walks the community chats re-evaluating muted unread
    /// counters and pinned/mention notifications.
    fn on_community_dialogs_notification_settings_changed(
        &mut self,
        _dialog_ids: &[DialogId],
        _old_settings: &DialogNotificationSettings,
        _new_settings: &DialogNotificationSettings,
    ) {
    }

    /// `UserManager::get_bot_data(user_id)` as the `is_dialog_owner`
    /// user-dialog arm reads it (CommunityManager.cpp:2021-2024) — the
    /// dialog is its owner's iff the bot is known and can be edited.
    fn get_bot_can_be_edited(&self, _user_id: crate::user_manager::UserId) -> bool {
        false
    }

    /// `ChatManager::get_chat_status(chat_id)` — the basic-group arm of
    /// `is_dialog_owner` (CommunityManager.cpp:2025-2026).
    fn get_chat_status(&self, _chat_id: crate::chat_manager::ChatId) -> DialogParticipantStatus {
        DialogParticipantStatus::banned(0, String::new())
    }

    /// `ChatManager::get_channel_status(channel_id)` — the channel arm
    /// of `is_dialog_owner` (CommunityManager.cpp:2027-2028).
    fn get_channel_status(
        &self,
        _channel_id: crate::chat_manager::ChannelId,
    ) -> DialogParticipantStatus {
        DialogParticipantStatus::banned(0, String::new())
    }

    /// `ToggleCommunityPeerLinkQuery::send` (1747ecde1,
    /// CommunityManager.cpp:350-386) — `communities.togglePeerLink` over
    /// the input community and the dialog's read-access input peer; the
    /// wire flag band is `visible = !is_deleted && !is_hidden`,
    /// `hidden = !is_deleted && is_hidden`, `deleted = is_deleted`. The
    /// answer is a `Bool` whose value is ignored — the request settles
    /// through the change-settle (`on_change_community_dialogs`); the
    /// error arm runs the dialog-error convention first. The
    /// invoke-after chain `{{community_id}}` collapses (the port's
    /// NetQuery carries no dependency set).
    fn send_toggle_community_peer_link_query(
        &mut self,
        community_id: CommunityId,
        _input_community: telegram_api::InputChannel,
        _dialog_id: DialogId,
        _is_deleted: bool,
        _is_hidden: bool,
    ) {
        tdutils::log_info!("Send communities.togglePeerLink for {community_id:?} — [net-inert]");
    }

    /// `DialogManager::get_input_peer(dialog_id, AccessRights::Know)`
    /// (20fc28e23, CommunityManager.cpp:2116) — the member peer lookup
    /// whose null answer is the 400 "Member not found" band of the
    /// member pair (the lookup runs after the sender→dialog resolution
    /// the request half performs).
    fn get_input_peer_know(&mut self, dialog_id: DialogId) -> Option<telegram_api::InputPeer> {
        tdutils::log_info!("Look up the member peer for {dialog_id:?} — [dialog seam]");
        None
    }

    /// `ToggleCommunityParticipantBannedQuery::send` (20fc28e23,
    /// CommunityManager.cpp:541-568) —
    /// `communities.toggleParticipantBanned(flags, unban = !is_banned,
    /// input_community, participant)`; the Bool answer's value is
    /// ignored and the promise settles through the change-settle
    /// (`on_change_community_dialogs`). The invoke-after chain
    /// `{{community_id}}` (ChainId.h:43-45) collapses with the port's
    /// no-dependency-set NetQuery.
    fn send_toggle_participant_banned_query(
        &mut self,
        community_id: CommunityId,
        input_community: telegram_api::InputChannel,
        participant: telegram_api::InputPeer,
        is_banned: bool,
    ) {
        tdutils::log_info!(
            "Send communities.toggleParticipantBanned for {community_id:?}, \
             is_banned = {is_banned} — [net-inert]"
        );
        let _ = (input_community, participant);
    }

    /// `GetCommunityParticipantJoinedChatsQuery::send` (4cd0dae69,
    /// CommunityManager.cpp:570-620) —
    /// `communities.getParticipantJoinedChats(input_community,
    /// participant)`; the answer builds the `communityMemberChats`
    /// object over the owned (creator) vs joined split.
    fn send_get_participant_joined_chats_query(
        &mut self,
        community_id: CommunityId,
        input_community: telegram_api::InputChannel,
        participant: telegram_api::InputPeer,
    ) {
        tdutils::log_info!(
            "Send communities.getParticipantJoinedChats for {community_id:?} — [net-inert]"
        );
        let _ = (input_community, participant);
    }

    /// `GetJoinedCommunitiesQuery::send` (CommunityManager.cpp:205-207)
    /// — the bare `communities.getJoinedCommunities`; the answer routes
    /// through the community net-answer drain (`GetJoinedCommunities`).
    fn send_get_joined_communities_query(&mut self) {
        tdutils::log_info!("Send communities.getJoinedCommunities — [net-inert]");
    }

    /// `ToggleCommunityCollapsedInDialogsQuery::send`
    /// (CommunityManager.cpp:511-540) —
    /// `communities.toggleCommunityCollapsedInDialogs` over
    /// `get_input_community`; the answer is an Updates that re-enters
    /// the updates intake (`on_get_updates`), so the collapse state
    /// comes back through the normal community intake, not the
    /// callback. The resolved input channel rides the seam (the
    /// manager is taken out under the env).
    fn send_toggle_community_collapsed_in_dialogs_query(
        &mut self,
        input_community: telegram_api::InputChannel,
        are_dialogs_collapsed: bool,
    ) {
        tdutils::log_info!(
            "Send communities.toggleCommunityCollapsedInDialogs(collapsed = \
             {are_dialogs_collapsed}) — [net-inert]"
        );
        let _ = input_community;
    }
}

/// `CommunityManager::Community` — the cached community record. The
/// byte-exact store/parse is recorded above.
#[derive(Debug, Default)]
pub struct Community {
    pub access_hash: i64,
    pub title: String,
    pub status: DialogParticipantStatus,
    pub date: i32,
    pub collapsed_in_dialogs: bool,
    pub is_changed: bool,
    /// The `Community::notification_settings` copy (a45f737cf) — a copy
    /// of `CommunityFull::notification_settings` that survives without
    /// the full; the `has_notification_settings` store flag rides the
    /// recorded store/parse collapse.
    pub notification_settings: DialogNotificationSettings,
}

/// `CommunityManager::CommunityDialog` — a linked chat.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CommunityDialog {
    dialog_id: DialogId,
    can_view_history: bool,
    is_visible: bool,
}

impl CommunityDialog {
    /// The wire ctor (CommunityManager.cpp:257-260).
    pub fn from_telegram(peer: &telegram_api::CommunityPeer) -> CommunityDialog {
        let dialog_id = peer
            .peer
            .as_ref()
            .map_or(DialogId::default(), DialogId::from_peer);
        CommunityDialog {
            dialog_id,
            can_view_history: peer.can_view_history,
            is_visible: peer.visible,
        }
    }

    pub fn is_valid(&self) -> bool {
        self.dialog_id.is_valid()
    }

    /// `get_community_chat_object` (CommunityManager.cpp:265-269).
    pub fn get_community_chat_object(
        &self,
        env: &mut impl CommunityManagerEnv,
    ) -> td_api::CommunityChat {
        td_api::CommunityChat {
            chat_id: env.get_chat_id_object(self.dialog_id),
            can_view_history: self.can_view_history,
            is_hidden: !self.is_visible,
        }
    }
}

/// `CommunityManager::CommunityFull` — the full record (the store/parse
/// is recorded above).
#[derive(Debug, Default, PartialEq)]
pub struct CommunityFull {
    pub about: String,
    pub dialogs: Vec<CommunityDialog>,
    pub administrator_count: i32,
    pub banned_count: i32,
    pub peer_link_requests_pending: i32,
    /// `CommunityFull::notification_settings` (99302f0e2) — the
    /// `communityFullInfo.notification_settings` source.
    pub notification_settings: DialogNotificationSettings,
}

/// The collapsed `CommunityManager`.
#[derive(Default)]
pub struct CommunityManager {
    communities: std::collections::BTreeMap<i64, Community>,
    communities_full: std::collections::BTreeMap<i64, CommunityFull>,
    /// The `td_api::CommunityId`-typed promises parked at the create
    /// seam (the answer chains behind `on_get_updates` into
    /// `finish_create_community`).
    pub parked_create_promises: Vec<Promise<td_api::CommunityId>>,
    /// The `()`-typed promises parked at the rename seam (the
    /// `channels.editTitle` answer family).
    pub pending_rename: Vec<Promise<()>>,
    /// The `EditCommunityDefaultBannedRightsQuery` promise lot.
    pub pending_permissions: Vec<Promise<()>>,
    /// The `DeleteCommunityQuery` promise lot.
    pub pending_delete: Vec<Promise<()>>,
    /// The `UpdateCommunityNotifySettingsQuery` promises keyed by
    /// community (`setCommunityNotificationSettings`; the C++ promise
    /// rides its own query — the answer kind's community_id pairs it
    /// exactly, unlike the single-community FIFO lots above).
    ///
    /// Lot-pairing divergence for the keyed lots that pop first-match
    /// (this lot and the mutator lots below — `pending_toggle_peer_link`,
    /// `pending_ban_toggles`, `pending_member_chats`,
    /// `pending_peer_link_requests`, `pending_process_request`,
    /// `pending_process_all_requests`): the C++ parks each mutator's
    /// promise in its own query handler (per-query pairing), while the
    /// port's keyed lots pop first-match per community, so two
    /// concurrent same-community requests can swap error outcomes
    /// between their promises (the ok-paths are indistinguishable) — a
    /// recorded divergence, consistent with the QueryCombiner
    /// approximation note at `pending_change_dialogs`.
    pub pending_set_notification_settings: Vec<(i64, Promise<()>)>,
    /// The `on_change_community_dialogs` promises keyed by community —
    /// the change-settle parks them while the community full reloads; the
    /// shared reload tail (`on_reload_community_full_for_notification_settings`,
    /// which both reload outcomes funnel through) drains them regardless
    /// of the outcome. The C++ promise rides the
    /// `get_community_full_queries_` QueryCombiner, which merges
    /// concurrent reloads and settles every added promise at the single
    /// answer, so the whole keyed entry drains at once.
    pub pending_change_dialogs: Vec<(i64, Promise<()>)>,
    /// The `ToggleCommunityPeerLinkQuery` promises keyed by community —
    /// the C++ handler carries its own promise, and the answer kind's
    /// community_id pairs the toggle with its settle (the change-settle
    /// either resolves it now or moves it onto `pending_change_dialogs`).
    pub pending_toggle_peer_link: Vec<(i64, Promise<()>)>,
    /// The `ToggleCommunityParticipantBannedQuery` promises keyed by
    /// community (`toggleCommunityMemberIsBanned`) — the Bool answer's
    /// value is ignored and the promise settles through the
    /// change-settle with the query's own community, so the lot pairs
    /// the answer with its community's promise.
    pub pending_ban_toggles: Vec<(i64, Promise<()>)>,
    /// The `GetCommunityParticipantJoinedChatsQuery` promises keyed by
    /// community (`getCommunityMemberChats`) — the answer builds the
    /// per-community object, so concurrent settles pair with their own
    /// answer.
    pub pending_member_chats: Vec<(i64, Promise<td_api::CommunityMemberChats>)>,
    /// The `ToggleCommunityCollapsedInDialogsQuery` promise lot (the
    /// collapse toggle's promise settles on the Updates answer, parked
    /// until the update lands).
    pub pending_collapse: Vec<Promise<()>>,
    /// The `GetCommunityPeerLinkRequestsQuery` promises keyed by
    /// community (`getCommunityAddChatRequests`) — the answer kind's
    /// community_id pairs the page with its own query, exactly like the
    /// settings lot above.
    pub pending_peer_link_requests: Vec<(i64, Promise<td_api::CommunityAddChatRequests>)>,
    /// The `ProcessCommunityPeerLinkRequestQuery` promises keyed by
    /// community (`processCommunityAddChatRequest`) — the answer pops
    /// one and settles it through the change-settle.
    pub pending_process_request: Vec<(i64, Promise<()>)>,
    /// The `ProcessCommunityPeerLinkRequestsQuery` promises keyed by
    /// community (`processCommunityAddChatRequests`) — the answer pops
    /// one and settles it through the change-settle.
    pub pending_process_all_requests: Vec<(i64, Promise<()>)>,
    /// `get_community_notification_settings_queries_` (ea8219749,
    /// 09634ee46) — the communities with an in-flight
    /// `GetCommunityNotifySettingsQuery`.
    get_community_notification_settings_queries: std::collections::BTreeSet<i64>,
    /// `pending_community_notification_settings_` (c9ed2c390) — the
    /// settings parked until the community's chat list loads.
    pending_community_notification_settings:
        std::collections::BTreeMap<i64, DialogNotificationSettings>,
    /// The `community_unmute_timeout_` MultiTimeout slots (9a16c4396,
    /// d46d51f61); the value is the absolute process-time deadline.
    community_unmute_timeout: std::collections::BTreeMap<i64, f64>,
    /// The `GetJoinedCommunitiesQuery` promise lot — the promise parks
    /// at the send and settles on the net answer (the C++ query carries
    /// its own).
    pub pending_joined_communities: Vec<Promise<td_api::Communities>>,
}

/// A keyed-lot slot — the `CommunityManager` field accessor the
/// first-match pop routes through (an alias because the inline HRTB
/// fn-pointer spelling trips `clippy::type_complexity`).
type KeyedLotSlot<T> = for<'a> fn(&'a mut CommunityManager) -> &'a mut Vec<(i64, Promise<T>)>;

impl CommunityManager {
    /// Pop the head promise of a family lot — the answer-routing half of
    /// a query family (the C++ handler's own promise).
    pub(crate) fn take_promise<T: std::marker::Send>(
        &mut self,
        slot: for<'a> fn(&'a mut CommunityManager) -> &'a mut Vec<Promise<T>>,
    ) -> Option<Promise<T>> {
        let lot = slot(self);
        if lot.is_empty() {
            None
        } else {
            Some(lot.remove(0))
        }
    }

    /// Pop the first promise parked under the community in a keyed lot
    /// — the answer-routing half of a keyed query family (the C++
    /// handler parks the promise in its own query; the port's
    /// first-match pop pairs FIFO within the key — the lot-pairing
    /// divergence is recorded at the keyed-lot declarations).
    pub(crate) fn take_keyed_promise<T: std::marker::Send>(
        &mut self,
        community_id: CommunityId,
        slot: KeyedLotSlot<T>,
    ) -> Option<Promise<T>> {
        let lot = slot(self);
        lot.iter()
            .position(|(id, _)| *id == community_id.get())
            .map(|index| lot.remove(index).1)
    }

    pub fn new() -> CommunityManager {
        CommunityManager::default()
    }

    /// `have_community` (CommunityManager.cpp).
    pub fn have_community(&self, community_id: CommunityId) -> bool {
        self.communities.contains_key(&community_id.get())
    }

    fn add_community(&mut self, community_id: CommunityId, source: &str) -> &mut Community {
        let _ = source;
        self.communities
            .entry(community_id.get())
            .or_insert_with(|| Community {
                is_changed: true,
                ..Community::default()
            })
    }

    fn get_community(&self, community_id: CommunityId) -> Option<&Community> {
        self.communities.get(&community_id.get())
    }

    fn add_community_full(&mut self, community_id: CommunityId) -> &mut CommunityFull {
        self.communities_full.entry(community_id.get()).or_default()
    }

    /// `on_get_community` (CommunityManager.cpp:728-810) — the wire
    /// intake (the photo + default-permission planes are the recorded
    /// seams).
    pub fn on_get_community(
        &mut self,
        env: &mut impl CommunityManagerEnv,
        community: &telegram_api::community,
        source: &str,
    ) {
        let community_id = CommunityId::new(community.id);
        if !community_id.is_valid() {
            tdutils::log_error!("Receive invalid {community_id:?} from {source}");
            return;
        }
        if community.flags == 0 && community.access_hash == 0 && community.title.is_empty() {
            return;
        }
        let is_min = community.min;
        let access_hash = community.access_hash;
        if access_hash == 0 && !is_min {
            tdutils::log_error!(
                "Receive non-min {community_id:?} without access_hash from {source}"
            );
            return;
        }
        let status = if community.creator {
            DialogParticipantStatus::creator(!community.left, false, String::new())
        } else if let Some(admin_rights) = &community.admin_rights {
            DialogParticipantStatus::administrator(
                AdministratorRights::from_chat_admin_rights(
                    Some(admin_rights),
                    crate::chat_manager::ChannelType::Unknown,
                ),
                String::new(),
                false,
            )
        } else if community.left {
            DialogParticipantStatus::left()
        } else {
            DialogParticipantStatus::member(0, String::new())
        };

        let community_record = self.add_community(community_id, source);
        if community_record.access_hash != access_hash
            && (!is_min || community_record.access_hash == 0)
        {
            community_record.access_hash = access_hash;
            community_record.is_changed = true;
        }
        if community_record.date != community.date && (!is_min || community_record.date == 0) {
            community_record.date = community.date;
            community_record.is_changed = true;
        }
        if community_record.collapsed_in_dialogs != community.collapsed_in_dialogs && !is_min {
            community_record.collapsed_in_dialogs = community.collapsed_in_dialogs;
            community_record.is_changed = true;
            if !community_record.collapsed_in_dialogs {
                // ea2241d48 — expanding the chats removes the community
                // from all chat folders
                env.remove_community_from_dialog_filters(community_id);
            }
        }
        if !is_min {
            if community_record.status != status {
                community_record.status = status;
                community_record.is_changed = true;
            }
            if community_record.title != community.title {
                community_record.title = community.title.clone();
                community_record.is_changed = true;
            }
        }
        if community_record.is_changed {
            community_record.is_changed = false;
            env.send_update(self.get_update_community_object(community_id));
        }
    }

    /// `on_get_community_forbidden` (CommunityManager.cpp).
    pub fn on_get_community_forbidden(&mut self, community_id: CommunityId, source: &str) {
        let _ = source;
        self.communities.remove(&community_id.get());
        self.communities_full.remove(&community_id.get());
    }

    /// `on_get_community_full` (CommunityManager.cpp:1010-1054).
    pub fn on_get_community_full(
        &mut self,
        env: &mut impl CommunityManagerEnv,
        community: &telegram_api::communityFull,
    ) {
        let community_id = CommunityId::new(community.id);
        if self.get_community(community_id).is_none() {
            tdutils::log_error!("Can't find {community_id:?}");
            return;
        }
        let community_full = self.add_community_full(community_id);
        let community_dialogs: Vec<CommunityDialog> = community
            .linked_peers
            .iter()
            .map(CommunityDialog::from_telegram)
            .filter(|dialog| dialog.is_valid())
            .collect();
        let is_changed = community_full.about != community.about
            || community_full.dialogs != community_dialogs
            || community_full.administrator_count != community.admins_count
            || community_full.banned_count != community.kicked_count
            || community_full.peer_link_requests_pending != community.peer_link_requests_pending;
        if is_changed {
            community_full.about = community.about.clone();
            community_full.dialogs = community_dialogs;
            community_full.administrator_count = community.admins_count;
            community_full.banned_count = community.kicked_count;
            community_full.peer_link_requests_pending = community.peer_link_requests_pending;
        }
        if is_changed {
            let update = self.get_update_community_full_info_object(env, community_id);
            env.send_update(update);
        }
        // 99302f0e2 + 09634ee46: the wire `communityFull` carries no
        // notify settings, so an unsynchronized full triggers the settings
        // query.
        if !self
            .communities_full
            .get(&community_id.get())
            .is_some_and(|full| full.notification_settings.is_synchronized)
            && !env.auth_is_bot()
        {
            // The C++ CHECK band: the cached community exists (the intake
            // gate above).
            if let Some(input_community) =
                self.get_input_community_impl(env.auth_is_bot(), community_id)
            {
                env.send_get_community_notify_settings_query(community_id, input_community);
            }
        }
        // c9ed2c390: the settings parked at the missing full apply now
        // that the chat list is here (the `on_reload_community_full_`
        // promise chain).
        self.on_reload_community_full_for_notification_settings(env, community_id);
    }

    /// `on_update_community_notify_settings` (99302f0e2, a45f737cf,
    /// 5ab6224b7; CommunityManager.cpp:1560-1621) — the
    /// `updateNotifySettings`/get-dialogs intake. The `get_community_force`
    /// db/net load collapses to the memory lookup, so 5ab6224b7's
    /// load-first chaining (the `load_community_from_database_queries_`
    /// re-entry) is site-less.
    pub fn on_update_community_notify_settings(
        &mut self,
        env: &mut impl CommunityManagerEnv,
        community_id: CommunityId,
        peer_notify_settings: Option<&telegram_api::PeerNotifySettings>,
        _source: &str,
    ) {
        if env.auth_is_bot() {
            return;
        }
        if !self.communities.contains_key(&community_id.get()) {
            return;
        }
        let notification_settings = get_dialog_notification_settings_from_telegram(
            peer_notify_settings,
            self.communities
                .get(&community_id.get())
                .map(|community| &community.notification_settings),
            env.unix_time(),
        );
        if !notification_settings.is_synchronized {
            return;
        }

        if self.communities_full.contains_key(&community_id.get()) {
            self.update_community_full_notification_settings(
                env,
                community_id,
                notification_settings.clone(),
                "on_update_community_notify_settings",
            );
        }
        self.update_community_notification_settings(env, community_id, notification_settings);
    }

    /// `need_update_community_dialogs` (c9ed2c390,
    /// CommunityManager.cpp:1641-1656) — whether the change
    /// can affect the notification state of the community chats
    /// (maintained by MessagesManager); a `-1` triple is the
    /// unsynchronized/`use_default` "unknown" state.
    pub fn need_update_community_dialogs(
        old_settings: &DialogNotificationSettings,
        new_settings: &DialogNotificationSettings,
    ) -> bool {
        fn get_state(settings: &DialogNotificationSettings) -> (i32, i32, i32) {
            if !settings.is_synchronized {
                return (-1, -1, -1);
            }
            let mute = if settings.use_default_mute_until {
                -1
            } else {
                i32::from(settings.mute_until != 0)
            };
            let pinned = if settings.use_default_disable_pinned_message_notifications {
                -1
            } else {
                i32::from(settings.disable_pinned_message_notifications)
            };
            let mention = if settings.use_default_disable_mention_notifications {
                -1
            } else {
                i32::from(settings.disable_mention_notifications)
            };
            (mute, pinned, mention)
        }
        get_state(old_settings) != get_state(new_settings)
    }

    /// `update_community_notification_settings` (a45f737cf, da8ab91c4,
    /// c9ed2c390; CommunityManager.cpp:1637-1668) — the Community-cache
    /// half; the change is postponed when the community's chat list is
    /// needed but not loaded.
    pub fn update_community_notification_settings(
        &mut self,
        env: &mut impl CommunityManagerEnv,
        community_id: CommunityId,
        mut new_settings: DialogNotificationSettings,
    ) {
        new_settings.fix_mute_until(env.auth_is_bot(), env.unix_time());
        let current_settings = self
            .communities
            .get(&community_id.get())
            .map(|community| community.notification_settings.clone());
        let Some(current_settings) = current_settings else {
            // The C++ CHECK(c != nullptr) band: every caller holds a
            // cached community.
            return;
        };
        let need_update =
            need_update_dialog_notification_settings(&current_settings, &new_settings);
        if !need_update.are_changed && !need_update.need_update_local {
            self.pending_community_notification_settings
                .remove(&community_id.get());
            return;
        }

        if !env.auth_is_bot()
            && Self::need_update_community_dialogs(&current_settings, &new_settings)
            && !self.communities_full.contains_key(&community_id.get())
        {
            // the list of community chats is needed to apply the new
            // settings; the `get_community_full_force` db/net load
            // collapses to the memory lookup, so the full reload rides
            // the env seam
            if !self
                .pending_community_notification_settings
                .contains_key(&community_id.get())
                && let Some(input_community) =
                    self.get_input_community_impl(env.auth_is_bot(), community_id)
            {
                env.reload_community_full(community_id, input_community);
            }
            self.pending_community_notification_settings
                .insert(community_id.get(), new_settings);
            return;
        }

        self.pending_community_notification_settings
            .remove(&community_id.get());
        self.do_update_community_notification_settings(env, community_id, new_settings);
    }

    /// `on_reload_community_full_for_notification_settings` (c9ed2c390) —
    /// the parked settings apply once the full arrives. The C++ CHECK on
    /// the cached community reads as a tolerant return here (a community
    /// deleted while parked would crash the C++).
    pub fn on_reload_community_full_for_notification_settings(
        &mut self,
        env: &mut impl CommunityManagerEnv,
        community_id: CommunityId,
    ) {
        // The change-settle tail: the reload's ok outcome arrives here
        // through the `on_get_community_full` tail and its error outcome
        // through the answer arm's failed_reload branch, and the C++
        // settle lambda ignores the reload Result
        // (CommunityManager.cpp:2054-2057) — every promise parked by
        // `on_change_community_dialogs` settles ok either way.
        let pending = std::mem::take(&mut self.pending_change_dialogs);
        let mut settled = Vec::new();
        let mut kept = Vec::new();
        for (key, promise) in pending {
            if key == community_id.get() {
                settled.push(promise);
            } else {
                kept.push((key, promise));
            }
        }
        self.pending_change_dialogs = kept;
        for mut promise in settled {
            promise.set_value(());
        }
        let Some(mut new_settings) = self
            .pending_community_notification_settings
            .remove(&community_id.get())
        else {
            return;
        };
        let Some(community) = self.communities.get(&community_id.get()) else {
            return;
        };
        new_settings.fix_mute_until(env.auth_is_bot(), env.unix_time());
        let need_update = need_update_dialog_notification_settings(
            &community.notification_settings,
            &new_settings,
        );
        if need_update.are_changed || need_update.need_update_local {
            self.do_update_community_notification_settings(env, community_id, new_settings);
        }
    }

    /// `on_change_community_dialogs` (CommunityManager.cpp:2049-2058) —
    /// the change-settle every community-dialog-mutating query tail
    /// funnels into. Without a cached full the request settles now;
    /// otherwise the full reloads (`reload_community_full`, over the
    /// `get_community_full_queries_` QueryCombiner in C++) and the
    /// request settles ok when the reload answers, regardless of its
    /// outcome — the parked lot drains at the shared reload tail
    /// `on_reload_community_full_for_notification_settings`, so a reload
    /// error never propagates to the request.
    pub fn on_change_community_dialogs(
        &mut self,
        env: &mut impl CommunityManagerEnv,
        community_id: CommunityId,
        mut promise: Promise<()>,
    ) {
        if env.close_flag() {
            return promise.set_error(Status::error(500, "Request aborted"));
        }
        if !self.communities_full.contains_key(&community_id.get()) {
            return promise.set_value(());
        }
        // The C++ `reload_community_full` resolves `get_input_community`
        // into the reload promise, whose failure the settle lambda
        // swallows (CommunityManager.cpp:2054-2057) — a missing input
        // settles the request ok too, without a query. Unreachable in
        // practice (the full intake gates on the cached community; the
        // drop path clears both records), kept for the "regardless"
        // shape.
        let Some(input_community) = self.get_input_community_impl(env.auth_is_bot(), community_id)
        else {
            return promise.set_value(());
        };
        env.reload_community_full(community_id, input_community);
        self.pending_change_dialogs
            .push((community_id.get(), promise));
    }

    /// `is_dialog_owner` (f027a2d06, CommunityManager.cpp:2019-2034) — the
    /// add/remove owner test over the dialog type: a user dialog is its
    /// owner's iff the bot is known and can be edited, a basic group or a
    /// channel iff the user created it; secret chats and the empty dialog
    /// answer false. The cross-manager lookups ride the env seams.
    pub fn is_dialog_owner(&self, env: &impl CommunityManagerEnv, dialog_id: DialogId) -> bool {
        match dialog_id.get_type() {
            crate::dialog_id::DialogType::User => {
                env.get_bot_can_be_edited(dialog_id.get_user_id())
            }
            crate::dialog_id::DialogType::Chat => {
                env.get_chat_status(dialog_id.get_chat_id()).is_creator()
            }
            crate::dialog_id::DialogType::Channel => env
                .get_channel_status(dialog_id.get_channel_id())
                .is_creator(),
            crate::dialog_id::DialogType::SecretChat | crate::dialog_id::DialogType::None => false,
        }
    }

    /// `add_community_dialog` (1747ecde1 + the da0e19cf3 owner gate,
    /// CommunityManager.cpp:2005-2017) — the known-community gate, the
    /// Read access check, the owner test, then the visible/hidden-band
    /// `communities.togglePeerLink` send.
    pub fn add_community_dialog(
        &mut self,
        env: &mut impl CommunityManagerEnv,
        community_id: CommunityId,
        dialog_id: DialogId,
        is_hidden: bool,
        mut promise: Promise<()>,
    ) {
        if !self.have_community(community_id) {
            return promise.set_error(Status::error(400, "Community not found"));
        }
        if let Err(status) = env.check_dialog_access(dialog_id) {
            return promise.set_error(status);
        }
        if !self.is_dialog_owner(env, dialog_id) {
            return promise.set_error(Status::error(400, "Have not enough rights"));
        }
        // `ToggleCommunityPeerLinkQuery::send` resolves
        // `get_input_community` (the C++ CHECK band is the
        // have_community invariant above).
        let Some(input_community) = self.get_input_community(env, community_id) else {
            return promise.set_error(Status::error(500, "Community not found"));
        };
        env.send_toggle_community_peer_link_query(
            community_id,
            input_community,
            dialog_id,
            /* is_deleted = */ false,
            is_hidden,
        );
        self.pending_toggle_peer_link
            .push((community_id.get(), promise));
    }

    /// `remove_community_dialog` (f027a2d06, CommunityManager.cpp:2036-2047)
    /// — the get_community gate, the Read access check, the
    /// linked-peer-administrator OR dialog-owner rights gate, then the
    /// deleted-band `communities.togglePeerLink` send.
    pub fn remove_community_dialog(
        &mut self,
        env: &mut impl CommunityManagerEnv,
        community_id: CommunityId,
        dialog_id: DialogId,
        mut promise: Promise<()>,
    ) {
        let Some(community) = self.get_community(community_id) else {
            return promise.set_error(Status::error(400, "Community not found"));
        };
        if let Err(status) = env.check_dialog_access(dialog_id) {
            return promise.set_error(status);
        }
        // `get_community_status(c)` reads the stored status (the C++
        // `update_restrictions` tail collapses — the port's community
        // status reads are direct, as every sibling gate).
        if !community.status.can_manage_linked_peers_as_administrator()
            && !self.is_dialog_owner(env, dialog_id)
        {
            return promise.set_error(Status::error(400, "Have not enough rights"));
        }
        // `ToggleCommunityPeerLinkQuery::send` resolves
        // `get_input_community` (the C++ CHECK band is the cached-community
        // invariant above).
        let Some(input_community) = self.get_input_community(env, community_id) else {
            return promise.set_error(Status::error(500, "Community not found"));
        };
        env.send_toggle_community_peer_link_query(
            community_id,
            input_community,
            dialog_id,
            /* is_deleted = */ true,
            /* is_hidden = */ false,
        );
        self.pending_toggle_peer_link
            .push((community_id.get(), promise));
    }

    /// `get_community_add_dialog_requests` (ffc06ec75,
    /// CommunityManager.cpp:2060-2070) — the `getCommunityAddChatRequests`
    /// request half: the have_community gate, the positive-limit band,
    /// then the `GetCommunityPeerLinkRequestsQuery` send.
    pub fn get_community_add_dialog_requests(
        &mut self,
        env: &mut impl CommunityManagerEnv,
        community_id: CommunityId,
        offset: &str,
        limit: i32,
        mut promise: Promise<td_api::CommunityAddChatRequests>,
    ) {
        if !self.have_community(community_id) {
            return promise.set_error(Status::error(400, "Community not found"));
        }
        if limit <= 0 {
            return promise.set_error(Status::error(400, "Parameter limit must be positive"));
        }
        // `GetCommunityPeerLinkRequestsQuery::send` resolves
        // `get_input_community` (the C++ CHECK band is the
        // have_community invariant).
        let Some(input_community) = self.get_input_community(env, community_id) else {
            return promise.set_error(Status::error(500, "Community not found"));
        };
        env.send_get_community_peer_link_requests_query(
            community_id,
            input_community,
            offset,
            limit,
        );
        self.pending_peer_link_requests
            .push((community_id.get(), promise));
    }

    /// `GetCommunityPeerLinkRequestsQuery::on_result` (ffc06ec75,
    /// CommunityManager.cpp:411-434) — the page builder: the users +
    /// chats intakes run first, the total clamps up to the page size,
    /// invalid entries drop with the count decremented, the valid
    /// entries' dialogs force-create, and each page entry carries the
    /// hidden inversion (`is_hidden = !visible`) with the next offset
    /// passed through (an empty next offset means no more results).
    pub fn on_get_community_peer_link_requests(
        &mut self,
        env: &mut impl CommunityManagerEnv,
        mut ptr: telegram_api::communities_PeerLinkRequests,
        mut promise: Promise<td_api::CommunityAddChatRequests>,
    ) {
        env.on_get_users(
            std::mem::take(&mut ptr.users),
            "GetCommunityPeerLinkRequestsQuery",
        );
        env.on_get_chats(
            std::mem::take(&mut ptr.chats),
            "GetCommunityPeerLinkRequestsQuery",
        );

        let mut total_count = ptr.total_count;
        if total_count < ptr.requests.len() as i32 {
            tdutils::log_error!(
                "Receive wrong total count {} of {} requests",
                total_count,
                ptr.requests.len()
            );
            total_count = ptr.requests.len() as i32;
        }
        let mut requests = Vec::new();
        for request in &ptr.requests {
            let dialog_id = request
                .peer
                .as_ref()
                .map_or(DialogId::default(), DialogId::from_peer);
            let sender_user_id = crate::user_manager::UserId::new(request.requested_by);
            if !dialog_id.is_valid() || !sender_user_id.is_valid() {
                tdutils::log_error!("Receive invalid {request:?}");
                total_count -= 1;
                continue;
            }
            env.force_create_dialog(dialog_id, "GetCommunityPeerLinkRequestsQuery");
            requests.push(td_api::CommunityAddChatRequest {
                chat_id: env.get_chat_id_object(dialog_id),
                is_hidden: !request.visible,
                sender_user_id: env.get_user_id_object(sender_user_id, "communityAddChatRequest"),
                date: request.date,
            });
        }
        promise.set_value(td_api::CommunityAddChatRequests {
            total_count,
            requests,
            next_offset: ptr.next_offset,
        });
    }

    /// `process_community_add_dialog_request` (2b80bf533,
    /// CommunityManager.cpp:2072-2084) — the
    /// `processCommunityAddChatRequest` request half: the get_community
    /// gate, the admin-only linked-peer right, the read access on the
    /// requested chat, then the `ProcessCommunityPeerLinkRequestQuery`
    /// send (the requested chat's input peer resolves at the seam).
    pub fn process_community_add_dialog_request(
        &mut self,
        env: &mut impl CommunityManagerEnv,
        community_id: CommunityId,
        dialog_id: DialogId,
        approve: bool,
        mut promise: Promise<()>,
    ) {
        let Some(community) = self.get_community(community_id) else {
            return promise.set_error(Status::error(400, "Community not found"));
        };
        if !community.status.can_manage_linked_peers_as_administrator() {
            return promise.set_error(Status::error(400, "Have not enough rights"));
        }
        if let Err(status) = env.check_dialog_access(dialog_id) {
            return promise.set_error(status);
        }
        // `ProcessCommunityPeerLinkRequestQuery::send` resolves
        // `get_input_community` (the C++ CHECK band is the cached
        // community invariant).
        let Some(input_community) = self.get_input_community(env, community_id) else {
            return promise.set_error(Status::error(500, "Community not found"));
        };
        env.send_process_community_peer_link_request_query(
            community_id,
            dialog_id,
            input_community,
            approve,
        );
        self.pending_process_request
            .push((community_id.get(), promise));
    }

    /// `process_community_add_dialog_requests` (79a943019,
    /// CommunityManager.cpp:2086-2096) — the
    /// `processCommunityAddChatRequests` request half: the same community
    /// gates with no per-chat access check, then the
    /// `ProcessCommunityPeerLinkRequestsQuery` send.
    pub fn process_community_add_dialog_requests(
        &mut self,
        env: &mut impl CommunityManagerEnv,
        community_id: CommunityId,
        approve: bool,
        mut promise: Promise<()>,
    ) {
        let Some(community) = self.get_community(community_id) else {
            return promise.set_error(Status::error(400, "Community not found"));
        };
        if !community.status.can_manage_linked_peers_as_administrator() {
            return promise.set_error(Status::error(400, "Have not enough rights"));
        }
        let Some(input_community) = self.get_input_community(env, community_id) else {
            return promise.set_error(Status::error(500, "Community not found"));
        };
        env.send_process_community_peer_link_requests_query(community_id, input_community, approve);
        self.pending_process_all_requests
            .push((community_id.get(), promise));
    }

    /// `do_update_community_notification_settings` (c9ed2c390,
    /// 0063421d6, 09634ee46) — the committed write + the community-chat
    /// fan-out. `need_save_to_database` collapses into `is_changed` (no
    /// community db plane).
    fn do_update_community_notification_settings(
        &mut self,
        env: &mut impl CommunityManagerEnv,
        community_id: CommunityId,
        new_settings: DialogNotificationSettings,
    ) {
        let Some(community) = self.communities.get_mut(&community_id.get()) else {
            return;
        };
        let old_settings = std::mem::replace(&mut community.notification_settings, new_settings);
        // `need_save_to_database = true` collapses to nothing (no community
        // db plane) — the C++ `update_community` call only saves; the
        // `updateCommunity` update fires on `is_changed`, which a settings
        // change never sets.
        self.get_community_notification_settings_queries
            .remove(&community_id.get());
        let use_default_mute_until = community.notification_settings.use_default_mute_until;
        let mute_until = community.notification_settings.mute_until;
        self.schedule_community_unmute(env, community_id, use_default_mute_until, mute_until);
        self.update_community(env, community_id);

        if env.auth_is_bot()
            || !Self::need_update_community_dialogs(
                &old_settings,
                &self.communities[&community_id.get()].notification_settings,
            )
        {
            return;
        }
        let Some(community_full) = self.communities_full.get(&community_id.get()) else {
            tdutils::log_warning!("Failed to load the list of chats in {community_id:?}");
            return;
        };
        let dialog_ids: Vec<DialogId> = community_full
            .dialogs
            .iter()
            .map(|dialog| dialog.dialog_id)
            .collect();
        env.on_community_dialogs_notification_settings_changed(
            &dialog_ids,
            &old_settings,
            &self.communities[&community_id.get()].notification_settings,
        );
    }

    /// `update_community` (CommunityManager.cpp:1155-1213) — the collapsed
    /// visible effect: the `is_changed` consumption fires the
    /// `updateCommunity` update (the db/binlog halves are the recorded
    /// collapse).
    fn update_community(&mut self, env: &mut impl CommunityManagerEnv, community_id: CommunityId) {
        let is_changed = self
            .communities
            .get(&community_id.get())
            .is_some_and(|community| community.is_changed);
        if !is_changed {
            return;
        }
        if let Some(community) = self.communities.get_mut(&community_id.get()) {
            community.is_changed = false;
        }
        env.send_update(self.get_update_community_object(community_id));
    }

    /// `update_community_full_notification_settings` (a45f737cf,
    /// 99302f0e2) — the CommunityFull half; returns
    /// `need_update.need_update_server` (the set-arm's send gate).
    pub fn update_community_full_notification_settings(
        &mut self,
        env: &mut impl CommunityManagerEnv,
        community_id: CommunityId,
        new_settings: DialogNotificationSettings,
        _source: &str,
    ) -> bool {
        let Some(community_full) = self.communities_full.get_mut(&community_id.get()) else {
            // The C++ CHECK(community_full != nullptr) band: every caller
            // holds a loaded full.
            return false;
        };
        let need_update = need_update_dialog_notification_settings(
            &community_full.notification_settings,
            &new_settings,
        );
        if need_update.are_changed || need_update.need_update_local {
            community_full.notification_settings = new_settings;
        }
        let are_changed = need_update.are_changed;
        if are_changed {
            // `update_community_full` fires the updateCommunityFullInfo
            // update; the db save is the recorded collapse.
            let update = self.get_update_community_full_info_object(env, community_id);
            env.send_update(update);
        }
        need_update.need_update_server
    }

    /// `schedule_community_unmute` (9a16c4396, d46d51f61) — the MultiTimeout
    /// band; an expired mute schedules the immediate `on_community_unmute`
    /// (which updates the community chats).
    fn schedule_community_unmute(
        &mut self,
        env: &impl CommunityManagerEnv,
        community_id: CommunityId,
        use_default: bool,
        mute_until: i32,
    ) {
        let unix_time = env.unix_time();
        if !use_default && mute_until != 0 && mute_until < unix_time + 366 * 86400 {
            let timeout_in = f64::from((mute_until - unix_time + 1).max(0));
            self.community_unmute_timeout
                .insert(community_id.get(), env.now() + timeout_in);
        } else {
            self.community_unmute_timeout.remove(&community_id.get());
        }
    }

    /// `on_community_unmute` (9a16c4396, c9ed2c390) — the fired timeout
    /// clears an expired mute through the full update path.
    pub fn on_community_unmute(
        &mut self,
        env: &mut impl CommunityManagerEnv,
        community_id: CommunityId,
    ) {
        if env.auth_is_bot() {
            // just in case
            return;
        }
        let Some(community) = self.communities.get(&community_id.get()) else {
            return;
        };
        let notification_settings = &community.notification_settings;
        if notification_settings.use_default_mute_until
            || notification_settings.mute_until == 0
            || self
                .pending_community_notification_settings
                .contains_key(&community_id.get())
        {
            return;
        }

        let unix_time = env.unix_time();
        if notification_settings.mute_until > unix_time {
            tdutils::log_info!(
                "Failed to unmute {community_id:?} in {unix_time}, will be unmuted in {}",
                notification_settings.mute_until
            );
            self.schedule_community_unmute(
                env,
                community_id,
                false,
                notification_settings.mute_until,
            );
            return;
        }

        tdutils::log_info!("Unmute {community_id:?}");
        let mut new_settings = notification_settings.clone();
        new_settings.mute_until = 0;
        self.update_community_notification_settings(env, community_id, new_settings);
    }

    /// `get_community_notification_settings` (ea8219749, 09634ee46,
    /// 5ab6224b7; CommunityManager.cpp:1769-1791) — the settings read; an
    /// unsynchronized community
    /// triggers the query once. The C++ pointer return becomes a clone
    /// (the manager borrow ends at the read), and 5ab6224b7's
    /// load-in-flight gate is site-less (no community db plane).
    pub fn get_community_notification_settings(
        &mut self,
        env: &mut impl CommunityManagerEnv,
        community_id: CommunityId,
    ) -> Option<DialogNotificationSettings> {
        if env.auth_is_bot() {
            return None;
        }
        if !self.communities.contains_key(&community_id.get()) {
            // `get_community_force` collapsed to the memory lookup.
            return None;
        }
        let synchronized = self.communities[&community_id.get()]
            .notification_settings
            .is_synchronized;
        if !synchronized {
            if self
                .get_community_notification_settings_queries
                .insert(community_id.get())
            {
                // The C++ CHECK band: the community is cached (the gate
                // above).
                if let Some(input_community) =
                    self.get_input_community_impl(env.auth_is_bot(), community_id)
                {
                    env.send_get_community_notify_settings_query(community_id, input_community);
                }
            }
            return None;
        }
        Some(
            self.communities[&community_id.get()]
                .notification_settings
                .clone(),
        )
    }

    /// `on_get_community_notification_settings_error`
    /// (09634ee46; CommunityManager.cpp:1793-1795) — the
    /// failed query erases its map entry so the next read re-sends.
    pub fn on_get_community_notification_settings_error(&mut self, community_id: CommunityId) {
        self.get_community_notification_settings_queries
            .remove(&community_id.get());
    }

    /// `reset_all_community_notification_settings`
    /// (8d2a35e05; CommunityManager.cpp:1797-1814) — every
    /// cached community resets to the synchronized defaults.
    pub fn reset_all_community_notification_settings(
        &mut self,
        env: &mut impl CommunityManagerEnv,
    ) {
        let community_ids: Vec<i64> = self.communities.keys().copied().collect();
        for community_id_int in community_ids {
            let new_settings = DialogNotificationSettings {
                is_synchronized: true,
                ..DialogNotificationSettings::default()
            };
            let community_id = CommunityId::new(community_id_int);
            if self.communities_full.contains_key(&community_id_int) {
                self.update_community_full_notification_settings(
                    env,
                    community_id,
                    new_settings.clone(),
                    "reset_all_community_notification_settings",
                );
            }
            self.update_community_notification_settings(env, community_id, new_settings);
        }
    }

    /// `set_community_notification_settings` (99302f0e2,
    /// a45f737cf; CommunityManager.cpp:1831-1853) — the
    /// `setCommunityNotificationSettings` request half; the
    /// `get_community_full_force` load collapses to the memory lookup.
    pub fn set_community_notification_settings(
        &mut self,
        env: &mut impl CommunityManagerEnv,
        community_id: CommunityId,
        notification_settings: Option<&td_api::ChatNotificationSettings>,
        mut promise: Promise<()>,
    ) {
        if !self.have_community(community_id) {
            return promise.set_error(Status::error(400, "Community not found"));
        }
        if !self.communities_full.contains_key(&community_id.get()) {
            return promise.set_error(Status::error(
                400,
                "Full information about the community must be received first",
            ));
        }
        let old_settings = self.communities_full[&community_id.get()]
            .notification_settings
            .clone();
        let new_settings = match get_dialog_notification_settings_from_td_api(
            notification_settings,
            &old_settings,
            env.unix_time(),
        ) {
            Ok(new_settings) => new_settings,
            Err(status) => return promise.set_error(status),
        };
        self.update_community_notification_settings(env, community_id, new_settings.clone());
        let need_update_server = self.update_community_full_notification_settings(
            env,
            community_id,
            new_settings,
            "set_community_notification_settings",
        );
        if !need_update_server {
            return promise.set_value(());
        }
        // The wire form is the full's (already updated) settings. The C++
        // CHECK band: the community is cached (the have_community gate).
        let wire_settings = self.communities_full[&community_id.get()]
            .notification_settings
            .clone();
        if let Some(input_community) =
            self.get_input_community_impl(env.auth_is_bot(), community_id)
        {
            env.send_update_community_notify_settings_query(
                community_id,
                input_community,
                &wire_settings,
            );
            self.pending_set_notification_settings
                .push((community_id.get(), promise));
        } else {
            promise.set_error(Status::error(500, "Community not found"));
        }
    }

    /// The earliest community unmute deadline — the Td timeout slot merge
    /// point (the `community_unmute_timeout_` MultiTimeout heap; the
    /// slot map is key-ordered, so the minimum is walked explicitly).
    pub fn next_deadline(&self) -> Option<f64> {
        self.community_unmute_timeout
            .values()
            .copied()
            .reduce(f64::min)
    }

    /// The `community_unmute_timeout_` callback pump — fires every due
    /// community once (the C++ MultiTimeout callback routes into
    /// `on_community_unmute`).
    pub fn tick_timeouts(&mut self, env: &mut impl CommunityManagerEnv) {
        let now = env.now();
        let due: Vec<i64> = self
            .community_unmute_timeout
            .iter()
            .filter(|(_, deadline)| **deadline <= now)
            .map(|(&key, _)| key)
            .collect();
        for key in due {
            self.community_unmute_timeout.remove(&key);
            if env.close_flag() {
                return;
            }
            self.on_community_unmute(env, CommunityId::new(key));
        }
    }

    /// `create_community` (CommunityManager.cpp:1079-1084).
    pub fn create_community(
        &mut self,
        env: &mut impl CommunityManagerEnv,
        name: &str,
        dialog_id: DialogId,
        is_hidden: bool,
        mut promise: Promise<td_api::CommunityId>,
    ) {
        if let Err(status) = env.check_dialog_access(dialog_id) {
            return promise.set_error(status);
        }
        env.send_create_community_query(name, dialog_id, is_hidden);
        self.parked_create_promises.push(promise);
    }

    /// `finish_create_community` (CommunityManager.cpp:1086-1091).
    pub fn finish_create_community(
        &mut self,
        env: &mut impl CommunityManagerEnv,
        community_id: CommunityId,
        mut promise: Promise<td_api::CommunityId>,
    ) {
        if env.close_flag() {
            return promise.set_error(Status::error(500, "Request aborted"));
        }
        promise.set_value(td_api::CommunityId {
            id: community_id.get(),
        });
    }

    /// `set_community_name` (CommunityManager.cpp:1093-1107).
    pub fn set_community_name(
        &mut self,
        env: &mut impl CommunityManagerEnv,
        community_id: CommunityId,
        name: &str,
        mut promise: Promise<()>,
    ) {
        let Some(community) = self.get_community(community_id) else {
            return promise.set_error(Status::error(400, "Community not found"));
        };
        if !community.status.is_administrator() || !community.status.can_change_info_and_settings()
        {
            return promise.set_error(Status::error(
                400,
                "Not enough rights to change community name",
            ));
        }
        // `EditCommunityTitleQuery::send` resolves `get_input_community`
        // (the C++ CHECK band is the cached-community invariant above).
        let Some(input_community) = self.get_input_community(env, community_id) else {
            return promise.set_error(Status::error(500, "Community not found"));
        };
        env.send_set_community_name_query(input_community, name);
        self.pending_rename.push(promise);
    }

    /// `set_community_photo` gates (fb05f53c2,
    /// CommunityManager.cpp:1170-1183) — the rights half of the manager
    /// method; the `DialogManager::do_set_dialog_photo` ladder over
    /// `community_id.get_fake_dialog_id()` runs from the request arm (the
    /// port's Td-level photo plane).
    pub fn set_community_photo_gates(
        &self,
        community_id: CommunityId,
    ) -> std::result::Result<(), Status> {
        let Some(community) = self.get_community(community_id) else {
            return Err(Status::error(400, "Community not found"));
        };
        if !community.status.is_administrator() || !community.status.can_change_info_and_settings()
        {
            return Err(Status::error(400, "Have not enough rights"));
        }
        Ok(())
    }

    /// `set_community_permissions` (d55f0a694,
    /// CommunityManager.cpp:1219-1236).
    pub fn set_community_permissions(
        &mut self,
        env: &mut impl CommunityManagerEnv,
        community_id: CommunityId,
        permissions: Option<&td_api::CommunityPermissions>,
        mut promise: Promise<()>,
    ) {
        let Some(permissions) = permissions else {
            return promise.set_error(Status::error(400, "New permissions must be non-empty"));
        };
        let Some(community) = self.get_community(community_id) else {
            return promise.set_error(Status::error(400, "Community not found"));
        };
        if !community.status.can_restrict_members() {
            return promise.set_error(Status::error(400, "Have not enough rights"));
        }
        // `EditCommunityDefaultBannedRightsQuery::send` resolves the
        // community's input peer.
        let Some(input_peer) = self.get_input_peer(env, community_id) else {
            return promise.set_error(Status::error(500, "Community not found"));
        };
        env.send_set_community_permissions_query(
            input_peer,
            &RestrictedRights::from_community_permissions(Some(permissions)),
        );
        self.pending_permissions.push(promise);
    }

    /// `delete_community` (f5f7fe5fd, CommunityManager.cpp:1268-1278).
    pub fn delete_community(
        &mut self,
        env: &mut impl CommunityManagerEnv,
        community_id: CommunityId,
        mut promise: Promise<()>,
    ) {
        let Some(community) = self.get_community(community_id) else {
            return promise.set_error(Status::error(400, "Community not found"));
        };
        if !community.status.is_creator() {
            return promise.set_error(Status::error(400, "Have not enough rights"));
        }
        // `DeleteCommunityQuery::send` resolves `get_input_community`.
        let Some(input_community) = self.get_input_community(env, community_id) else {
            return promise.set_error(Status::error(500, "Community not found"));
        };
        env.send_delete_community_query(input_community);
        self.pending_delete.push(promise);
    }

    /// `toggle_community_participant_is_banned` (20fc28e23,
    /// CommunityManager.cpp:2107-2122) — the shared member gate ladder
    /// (the community gate, the restrict-members right, then the
    /// Know-peer "Member not found" band) over the participant-banned
    /// wire toggle. The C++ `get_community_status` updates the
    /// restriction deadlines first; the port reads the cached status
    /// raw (the recorded simplification the other rights gates share).
    pub fn toggle_community_participant_is_banned(
        &mut self,
        env: &mut impl CommunityManagerEnv,
        community_id: CommunityId,
        participant_dialog_id: DialogId,
        is_banned: bool,
        mut promise: Promise<()>,
    ) {
        let Some(community) = self.get_community(community_id) else {
            return promise.set_error(Status::error(400, "Community not found"));
        };
        if !community.status.can_restrict_members() {
            return promise.set_error(Status::error(400, "Have not enough rights"));
        }
        let Some(participant) = env.get_input_peer_know(participant_dialog_id) else {
            return promise.set_error(Status::error(400, "Member not found"));
        };
        // `ToggleCommunityParticipantBannedQuery::send` resolves
        // `get_input_community` (the C++ CHECK band is the gate above).
        let Some(input_community) = self.get_input_community(env, community_id) else {
            return promise.set_error(Status::error(500, "Community not found"));
        };
        env.send_toggle_participant_banned_query(
            community_id,
            input_community,
            participant,
            is_banned,
        );
        self.pending_ban_toggles.push((community_id.get(), promise));
    }

    /// `get_community_participant_dialogs` (4cd0dae69,
    /// CommunityManager.cpp:2124-2140) — the same member gate ladder
    /// over the participant-joined-chats query.
    pub fn get_community_participant_dialogs(
        &mut self,
        env: &mut impl CommunityManagerEnv,
        community_id: CommunityId,
        participant_dialog_id: DialogId,
        mut promise: Promise<td_api::CommunityMemberChats>,
    ) {
        let Some(community) = self.get_community(community_id) else {
            return promise.set_error(Status::error(400, "Community not found"));
        };
        if !community.status.can_restrict_members() {
            return promise.set_error(Status::error(400, "Have not enough rights"));
        }
        let Some(participant) = env.get_input_peer_know(participant_dialog_id) else {
            return promise.set_error(Status::error(400, "Member not found"));
        };
        // `GetCommunityParticipantJoinedChatsQuery::send` resolves
        // `get_input_community` (the same CHECK band).
        let Some(input_community) = self.get_input_community(env, community_id) else {
            return promise.set_error(Status::error(500, "Community not found"));
        };
        env.send_get_participant_joined_chats_query(community_id, input_community, participant);
        self.pending_member_chats
            .push((community_id.get(), promise));
    }

    /// `get_joined_communities` (CommunityManager.cpp:1853-1855) — the
    /// `communities.getJoinedCommunities` send; the promise parks until
    /// the answer drains through [`CommunityNetKind`](crate::td) (no new
    /// cache: the answer reads the existing community cache only).
    pub fn get_joined_communities(
        &mut self,
        env: &mut impl CommunityManagerEnv,
        promise: Promise<td_api::Communities>,
    ) {
        env.send_get_joined_communities_query();
        self.pending_joined_communities.push(promise);
    }

    /// `on_get_joined_communities` (CommunityManager.cpp:1857-1875) —
    /// the answer intake: the ids collect from the
    /// `telegram_api::community` chats only (anything else — including
    /// `communityForbidden` — logs the C++ error and skips the list),
    /// the chats intaken so the community dialogs exist before the id
    /// list resolves, then the cached ids answer in input order — an
    /// entry whose intake fails (the `on_get_community` gates) drops
    /// silently (the cpp-map §4 note).
    pub fn on_get_joined_communities(
        &mut self,
        env: &mut impl CommunityManagerEnv,
        chats: Vec<telegram_api::Chat>,
        mut promise: Promise<td_api::Communities>,
    ) {
        let mut community_ids = Vec::new();
        let mut other_chats = Vec::new();
        for chat in chats {
            match chat {
                telegram_api::Chat::community(community) => {
                    community_ids.push(CommunityId::new(community.id));
                    // The community-first intake pass (cpp:1867's
                    // `on_get_chats` ordering, ChatManager.cpp:5780-5796):
                    // the shared hop lives at `Td::intake_chats` (#143),
                    // but this method runs under the take-dance — the
                    // dispatcher holds the community manager out of
                    // `Td`, so routing through the seam would apply into
                    // the placeholder and the put-back would clobber it —
                    // so each community lands in the cache through the
                    // local routing BEFORE the `have_community` filter
                    // reads it.
                    self.on_get_community(env, &community, "on_get_joined_communities");
                }
                ref chat @ telegram_api::Chat::communityForbidden(ref forbidden) => {
                    tdutils::log_error!("Receive {chat:?} as a joined community");
                    self.on_get_community_forbidden(
                        CommunityId::new(forbidden.id),
                        "on_get_joined_communities",
                    );
                }
                chat => {
                    tdutils::log_error!("Receive {chat:?} as a joined community");
                    other_chats.push(chat);
                }
            }
        }
        env.on_get_chats(other_chats, "on_get_joined_communities");

        let result = td_api::Communities {
            community_ids: community_ids
                .into_iter()
                .filter(|community_id| self.have_community(*community_id))
                .map(|community_id| {
                    self.get_community_id_object(community_id, "on_get_joined_communities")
                })
                .collect(),
        };
        promise.set_value(result);
    }

    /// `get_community_id_object` (CommunityManager.cpp:2174-2181) — the
    /// id form. The unknown-community band (the `unknown_communities_`
    /// one-time `updateUnknownCommunity` emission + LOG(ERROR)) is a
    /// recorded divergence (the module doc): the port returns the bare
    /// id — every current caller pre-filters by `have_community`, so the
    /// band is unreachable through the real wiring (the joined-communities
    /// filter drops an unknown id before this call, exactly the C++
    /// cpp:1869-1874 shape).
    pub fn get_community_id_object(&self, community_id: CommunityId, _source: &str) -> i64 {
        community_id.get()
    }

    /// `get_input_community` (1118a50df, CommunityManager.cpp:1373-1385).
    pub fn get_input_community(
        &self,
        env: &impl CommunityManagerEnv,
        community_id: CommunityId,
    ) -> Option<telegram_api::InputChannel> {
        self.get_input_community_impl(env.auth_is_bot(), community_id)
    }

    /// The `get_input_community` body over the bot flag — the env-free form
    /// for the callers that hold the flag without the full community env
    /// (the MessageQueryManager search arm).
    pub(crate) fn get_input_community_impl(
        &self,
        auth_is_bot: bool,
        community_id: CommunityId,
    ) -> Option<telegram_api::InputChannel> {
        let mut access_hash = 0;
        match self.get_community(community_id) {
            Some(community) => access_hash = community.access_hash,
            None => {
                if !auth_is_bot || !community_id.is_valid() {
                    return None;
                }
            }
        }
        Some(telegram_api::InputChannel::inputChannel(
            telegram_api::inputChannel {
                channel_id: community_id.get(),
                access_hash,
            },
        ))
    }

    /// `get_input_peer` (1118a50df, CommunityManager.cpp:1387-1398).
    pub fn get_input_peer(
        &self,
        env: &impl CommunityManagerEnv,
        community_id: CommunityId,
    ) -> Option<telegram_api::InputPeer> {
        let mut access_hash = 0;
        match self.get_community(community_id) {
            Some(community) => access_hash = community.access_hash,
            None => {
                if !env.auth_is_bot() || !community_id.is_valid() {
                    return None;
                }
            }
        }
        Some(telegram_api::InputPeer::inputPeerChannel(
            telegram_api::inputPeerChannel {
                channel_id: community_id.get(),
                access_hash,
            },
        ))
    }

    /// `get_community_object` (CommunityManager.cpp) — the td_api form
    /// (the photo leg answers `None` — recorded).
    pub fn get_community_object(&self, community_id: CommunityId) -> Option<td_api::Community> {
        let community = self.get_community(community_id)?;
        Some(td_api::Community {
            id: community_id.get(),
            have_access: !community.status.is_left(),
            name: community.title.clone(),
            photo: None,
            date: community.date,
            status: Some(Self::get_community_member_status_object(&community.status)),
            permissions: Some(td_api::CommunityPermissions {
                can_edit_chat_list: !community.collapsed_in_dialogs,
            }),
            // `c->collapsed_in_dialogs` (CommunityManager.cpp:2195).
            are_chats_collapsed: community.collapsed_in_dialogs,
        })
    }

    /// The wire-status → td_api mapping (the C++ derives it from the
    /// participant status).
    fn get_community_member_status_object(
        status: &DialogParticipantStatus,
    ) -> td_api::CommunityMemberStatus {
        if status.is_creator() {
            td_api::CommunityMemberStatus::communityMemberStatusCreator(
                td_api::communityMemberStatusCreator,
            )
        } else if status.is_administrator() {
            td_api::CommunityMemberStatus::communityMemberStatusAdministrator(
                td_api::communityMemberStatusAdministrator {
                    can_be_edited: status.can_be_edited(),
                    rights: None,
                },
            )
        } else if status.is_left() {
            td_api::CommunityMemberStatus::communityMemberStatusLeft(
                td_api::communityMemberStatusLeft,
            )
        } else {
            td_api::CommunityMemberStatus::communityMemberStatusMember(
                td_api::communityMemberStatusMember,
            )
        }
    }

    /// `get_update_community_object` (CommunityManager.cpp).
    pub fn get_update_community_object(&self, community_id: CommunityId) -> td_api::Update {
        td_api::Update::updateCommunity(td_api::updateCommunity {
            community: self.get_community_object(community_id),
        })
    }

    /// `get_community_full_info_object` (CommunityManager.cpp).
    pub fn get_community_full_info_object(
        &self,
        env: &mut impl CommunityManagerEnv,
        community_id: CommunityId,
    ) -> Option<td_api::CommunityFullInfo> {
        let community_full = self.communities_full.get(&community_id.get())?;
        Some(td_api::CommunityFullInfo {
            photo: None,
            chats: community_full
                .dialogs
                .iter()
                .map(|dialog| dialog.get_community_chat_object(env))
                .collect(),
            administrator_count: community_full.administrator_count,
            banned_count: community_full.banned_count,
            add_chat_request_count: community_full.peer_link_requests_pending,
            // 99302f0e2: `communityFullInfo.notification_settings` — the
            // td_api object form of the full's settings.
            notification_settings: Some(get_chat_notification_settings_object(
                &community_full.notification_settings,
                env.unix_time(),
            )),
        })
    }

    /// `get_update_community_full_info_object` (CommunityManager.cpp).
    pub fn get_update_community_full_info_object(
        &self,
        env: &mut impl CommunityManagerEnv,
        community_id: CommunityId,
    ) -> td_api::Update {
        td_api::Update::updateCommunityFullInfo(td_api::updateCommunityFullInfo {
            community_id: community_id.get(),
            community_full_info: self.get_community_full_info_object(env, community_id),
        })
    }

    /// `get_current_state` (Requests.cpp:2205 area).
    pub fn get_current_state(&self, updates: &mut Vec<td_api::Update>) {
        for community_id in self.communities.keys() {
            updates.push(self.get_update_community_object(CommunityId::new(*community_id)));
        }
    }

    /// `toggle_community_are_dialogs_collapsed`
    /// (CommunityManager.cpp:2098-2105) — the known-community gate only
    /// (no rights gate); the `communities
    /// .toggleCommunityCollapsedInDialogs` send rides the net step and
    /// the promise settles on the Updates answer (the collapse state
    /// comes back through `on_get_community`).
    pub fn toggle_community_are_dialogs_collapsed(
        &mut self,
        env: &mut impl CommunityManagerEnv,
        community_id: CommunityId,
        are_dialogs_collapsed: bool,
        mut promise: Promise<()>,
    ) {
        if !self.have_community(community_id) {
            return promise.set_error(Status::error(400, "Community not found"));
        }
        // `ToggleCommunityCollapsedInDialogsQuery::send` resolves
        // `get_input_community` (the C++ CHECK band is the
        // cached-community invariant above).
        let Some(input_community) = self.get_input_community(env, community_id) else {
            return promise.set_error(Status::error(500, "Community not found"));
        };
        env.send_toggle_community_collapsed_in_dialogs_query(
            input_community,
            are_dialogs_collapsed,
        );
        self.pending_collapse.push(promise);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestEnv {
        updates: Vec<td_api::Update>,
        queries: Vec<String>,
        unix_time: i32,
        now: f64,
        reloaded_fulls: Vec<i64>,
        /// The fan-out seam records (dialog ids, old mute_until, new
        /// mute_until).
        fanouts: Vec<(Vec<DialogId>, i32, i32)>,
        /// The dialogs the access seam refuses (the Read-check error
        /// band of the add/remove gate walks).
        failing_access: Vec<DialogId>,
        /// The dialogs the status lookups answer a creator for (the
        /// `is_dialog_owner` group/channel bands).
        creator_dialogs: Vec<DialogId>,
        /// The users the bot lookup knows as editable (the
        /// `is_dialog_owner` user band).
        editable_bots: Vec<i64>,
        /// The dialog ids whose `get_input_peer(Know)` resolves — the
        /// member band of the member pair.
        known_members: Vec<i64>,
        /// The shared chat-intake seam records (chat count, source).
        intaken_chats: Vec<(usize, String)>,
        /// The `remove_community_from_dialog_filters` fan-out records
        /// (the community ids whose expansion removed them from the
        /// chat folders).
        filter_removals: Vec<i64>,
        /// The users-intake seam records (user count, source).
        intaken_users: Vec<(usize, String)>,
        /// The force-create records (dialog id, source).
        forced_dialogs: Vec<(DialogId, String)>,
        /// The `check_dialog_access` failure knob (defaults to Ok).
        dialog_access_error: Option<Status>,
    }

    impl TestEnv {
        fn new() -> TestEnv {
            TestEnv {
                updates: Vec::new(),
                queries: Vec::new(),
                unix_time: 100,
                now: 100.0,
                reloaded_fulls: Vec::new(),
                fanouts: Vec::new(),
                failing_access: Vec::new(),
                creator_dialogs: Vec::new(),
                editable_bots: Vec::new(),
                known_members: Vec::new(),
                intaken_chats: Vec::new(),
                filter_removals: Vec::new(),
                intaken_users: Vec::new(),
                forced_dialogs: Vec::new(),
                dialog_access_error: None,
            }
        }
    }

    impl CommunityManagerEnv for TestEnv {
        fn close_flag(&self) -> bool {
            false
        }

        fn send_update(&mut self, update: td_api::Update) {
            self.updates.push(update);
        }

        fn check_dialog_access(&mut self, dialog_id: DialogId) -> std::result::Result<(), Status> {
            if self.failing_access.contains(&dialog_id) {
                Err(Status::error(400, "Can't access the chat"))
            } else {
                match &self.dialog_access_error {
                    Some(status) => Err(status.clone()),
                    None => Ok(()),
                }
            }
        }

        fn get_chat_id_object(&mut self, _dialog_id: DialogId) -> i64 {
            3
        }

        fn send_create_community_query(
            &mut self,
            name: &str,
            _dialog_id: DialogId,
            is_hidden: bool,
        ) {
            self.queries
                .push(format!("createCommunity {name} {is_hidden}"));
        }

        fn send_set_community_name_query(
            &mut self,
            input_community: telegram_api::InputChannel,
            name: &str,
        ) {
            let channel_id = match &input_community {
                telegram_api::InputChannel::inputChannel(channel) => channel.channel_id,
                _ => 0,
            };
            self.queries
                .push(format!("setCommunityName {channel_id} {name}"));
        }

        fn send_set_community_permissions_query(
            &mut self,
            input_peer: telegram_api::InputPeer,
            _permissions: &RestrictedRights,
        ) {
            let community_id = match &input_peer {
                telegram_api::InputPeer::inputPeerChannel(peer) => peer.channel_id,
                _ => 0,
            };
            self.queries
                .push(format!("setCommunityPermissions {community_id}"));
        }

        fn send_delete_community_query(&mut self, input_community: telegram_api::InputChannel) {
            let channel_id = match &input_community {
                telegram_api::InputChannel::inputChannel(channel) => channel.channel_id,
                _ => 0,
            };
            self.queries.push(format!("deleteCommunity {channel_id}"));
        }

        fn unix_time(&self) -> i32 {
            self.unix_time
        }

        fn now(&self) -> f64 {
            self.now
        }

        fn send_get_community_notify_settings_query(
            &mut self,
            community_id: CommunityId,
            _input_community: telegram_api::InputChannel,
        ) {
            self.queries
                .push(format!("getNotifySettings {}", community_id.get()));
        }

        fn send_update_community_notify_settings_query(
            &mut self,
            community_id: CommunityId,
            _input_community: telegram_api::InputChannel,
            _notification_settings: &DialogNotificationSettings,
        ) {
            self.queries
                .push(format!("updateNotifySettings {}", community_id.get()));
        }

        fn reload_community_full(
            &mut self,
            community_id: CommunityId,
            _input_community: telegram_api::InputChannel,
        ) {
            self.reloaded_fulls.push(community_id.get());
        }

        fn send_get_community_peer_link_requests_query(
            &mut self,
            community_id: CommunityId,
            _input_community: telegram_api::InputChannel,
            offset: &str,
            limit: i32,
        ) {
            self.queries.push(format!(
                "getPeerLinkRequests {} {offset} {limit}",
                community_id.get()
            ));
        }

        fn on_get_users(&mut self, users: Vec<telegram_api::User>, source: &str) {
            self.intaken_users.push((users.len(), source.to_owned()));
        }

        fn on_get_chats(&mut self, chats: Vec<telegram_api::Chat>, source: &str) {
            self.intaken_chats.push((chats.len(), source.to_owned()));
        }

        fn force_create_dialog(&mut self, dialog_id: DialogId, source: &str) {
            self.forced_dialogs.push((dialog_id, source.to_owned()));
        }

        fn send_process_community_peer_link_request_query(
            &mut self,
            community_id: CommunityId,
            dialog_id: DialogId,
            _input_community: telegram_api::InputChannel,
            approve: bool,
        ) {
            self.queries.push(format!(
                "togglePeerLinkRequestApproval {} {:?} reject={}",
                community_id.get(),
                dialog_id,
                !approve
            ));
        }

        fn send_process_community_peer_link_requests_query(
            &mut self,
            community_id: CommunityId,
            _input_community: telegram_api::InputChannel,
            approve: bool,
        ) {
            self.queries.push(format!(
                "toggleAllPeerLinkRequestApproval {} reject={}",
                community_id.get(),
                !approve
            ));
        }

        fn send_get_joined_communities_query(&mut self) {
            self.queries.push("getJoinedCommunities".to_owned());
        }

        fn on_community_dialogs_notification_settings_changed(
            &mut self,
            dialog_ids: &[DialogId],
            old_settings: &DialogNotificationSettings,
            new_settings: &DialogNotificationSettings,
        ) {
            self.fanouts.push((
                dialog_ids.to_vec(),
                old_settings.mute_until,
                new_settings.mute_until,
            ));
        }

        fn get_bot_can_be_edited(&self, user_id: crate::user_manager::UserId) -> bool {
            self.editable_bots.contains(&user_id.get())
        }

        fn get_chat_status(&self, chat_id: crate::chat_manager::ChatId) -> DialogParticipantStatus {
            if self
                .creator_dialogs
                .contains(&DialogId::from_chat_id(chat_id))
            {
                DialogParticipantStatus::creator(true, false, String::new())
            } else {
                DialogParticipantStatus::banned(0, String::new())
            }
        }

        fn get_channel_status(
            &self,
            channel_id: crate::chat_manager::ChannelId,
        ) -> DialogParticipantStatus {
            if self
                .creator_dialogs
                .contains(&DialogId::from_channel_id(channel_id))
            {
                DialogParticipantStatus::creator(true, false, String::new())
            } else {
                DialogParticipantStatus::banned(0, String::new())
            }
        }

        fn send_toggle_community_peer_link_query(
            &mut self,
            community_id: CommunityId,
            _input_community: telegram_api::InputChannel,
            dialog_id: DialogId,
            is_deleted: bool,
            is_hidden: bool,
        ) {
            self.queries.push(format!(
                "togglePeerLink {} {:?} deleted={is_deleted} hidden={is_hidden}",
                community_id.get(),
                dialog_id
            ));
        }

        fn get_input_peer_know(&mut self, dialog_id: DialogId) -> Option<telegram_api::InputPeer> {
            if self.known_members.contains(&dialog_id.get()) {
                Some(telegram_api::InputPeer::inputPeerUser(
                    telegram_api::inputPeerUser {
                        user_id: dialog_id.get(),
                        access_hash: 7,
                    },
                ))
            } else {
                None
            }
        }

        fn send_toggle_participant_banned_query(
            &mut self,
            community_id: CommunityId,
            input_community: telegram_api::InputChannel,
            participant: telegram_api::InputPeer,
            is_banned: bool,
        ) {
            let member = input_peer_tag(&participant);
            let band = if is_banned { "ban" } else { "unban" };
            self.queries.push(format!(
                "toggleParticipantBanned {} {member} {band}",
                community_id.get()
            ));
            let _ = input_community;
        }

        fn send_get_participant_joined_chats_query(
            &mut self,
            community_id: CommunityId,
            input_community: telegram_api::InputChannel,
            participant: telegram_api::InputPeer,
        ) {
            let member = input_peer_tag(&participant);
            self.queries.push(format!(
                "getParticipantJoinedChats {} {member}",
                community_id.get()
            ));
            let _ = input_community;
        }

        fn remove_community_from_dialog_filters(&mut self, community_id: CommunityId) {
            self.filter_removals.push(community_id.get());
        }

        fn send_toggle_community_collapsed_in_dialogs_query(
            &mut self,
            input_community: telegram_api::InputChannel,
            are_dialogs_collapsed: bool,
        ) {
            let channel_id = match &input_community {
                telegram_api::InputChannel::inputChannel(channel) => channel.channel_id,
                _ => 0,
            };
            self.queries.push(format!(
                "toggleCommunityCollapsedInDialogs {channel_id} {are_dialogs_collapsed}"
            ));
        }
    }

    /// The member-peer tag the member-pair query records share.
    fn input_peer_tag(peer: &telegram_api::InputPeer) -> String {
        match peer {
            telegram_api::InputPeer::inputPeerUser(user) => format!("user {}", user.user_id),
            telegram_api::InputPeer::inputPeerChannel(channel) => {
                format!("channel {}", channel.channel_id)
            }
            telegram_api::InputPeer::inputPeerChat(chat) => format!("chat {}", chat.chat_id),
            _ => "other".to_owned(),
        }
    }

    fn wire_community(id: i64, title: &str) -> telegram_api::community {
        telegram_api::community {
            flags: 1,
            creator: true,
            left: false,
            min: false,
            flags2: 0,
            collapsed_in_dialogs: false,
            id,
            access_hash: 5,
            title: title.to_string(),
            photo: None,
            date: 10,
            admin_rights: None,
            default_banned_rights: None,
        }
    }

    fn wire_peer() -> telegram_api::CommunityPeer {
        telegram_api::CommunityPeer {
            flags: 0,
            can_view_history: true,
            visible: true,
            peer: Some(telegram_api::Peer::peerChannel(telegram_api::peerChannel {
                channel_id: 3,
            })),
        }
    }

    #[test]
    fn on_get_community_caches_and_emits() {
        let mut env = TestEnv::new();
        let mut manager = CommunityManager::new();
        manager.on_get_community(&mut env, &wire_community(7, "My community"), "test");
        assert!(manager.have_community(CommunityId::new(7)));
        assert_eq!(env.updates.len(), 1);

        let object = manager.get_community_object(CommunityId::new(7)).unwrap();
        assert_eq!(object.name, "My community");
        assert!(object.have_access);
        assert!(matches!(
            object.status,
            Some(td_api::CommunityMemberStatus::communityMemberStatusCreator(
                _
            ))
        ));
    }

    #[test]
    fn forbidden_drops_the_record() {
        let mut env = TestEnv::new();
        let mut manager = CommunityManager::new();
        manager.on_get_community(&mut env, &wire_community(7, "My community"), "test");
        manager.on_get_community_forbidden(CommunityId::new(7), "test");
        assert!(!manager.have_community(CommunityId::new(7)));
    }

    #[test]
    fn create_parks_and_finish_settles() {
        let mut env = TestEnv::new();
        let mut manager = CommunityManager::new();
        let (promise, recorded) = record_id();
        manager.create_community(&mut env, "name", DialogId::new(1), true, promise);
        assert!(env.queries.iter().any(|q| q == "createCommunity name true"));
        assert_eq!(manager.parked_create_promises.len(), 1);
        let _ = recorded;

        let (promise, recorded) = record_id();
        manager.finish_create_community(&mut env, CommunityId::new(9), promise);
        assert_eq!(recorded.0.lock().unwrap().take().unwrap().id, 9);
    }

    #[test]
    fn joined_communities_answer_the_intake_scan_in_order() {
        // CommunityManager.cpp:1853-1875: the send parks the promise;
        // the answer intakes its chats BEFORE the id list resolves — the
        // community-family entries through the manager intake
        // (cpp:1867's `on_get_chats` routes them there,
        // ChatManager.cpp:5767-5773), so the `have_community` filter
        // (cpp:1869-1874) passes for communities that arrive in the
        // answer itself; non-community chats log the C++ error and ride
        // the chat seam; an entry whose intake fails drops silently.
        let mut env = TestEnv::new();
        let mut manager = CommunityManager::new();

        let (promise, recorded) = record_communities();
        manager.get_joined_communities(&mut env, promise);
        assert!(env.queries.iter().any(|q| q == "getJoinedCommunities"));
        assert_eq!(
            manager.pending_joined_communities.len(),
            1,
            "the promise parks at the send"
        );

        let promise = manager
            .take_promise(|m: &mut CommunityManager| &mut m.pending_joined_communities)
            .expect("the parked promise");
        let mut intake_failed = wire_member_community(8);
        intake_failed.access_hash = 0; // the non-min no-hash band — on_get_community drops it
        let chats = vec![
            telegram_api::Chat::community(wire_member_community(7)),
            telegram_api::Chat::community(intake_failed),
            telegram_api::Chat::chatForbidden(telegram_api::chatForbidden {
                id: 3,
                title: "forbidden".to_owned(),
            }),
            telegram_api::Chat::community(wire_member_community(9)),
        ];
        manager.on_get_joined_communities(&mut env, chats, promise);
        assert_eq!(
            env.intaken_chats,
            vec![(1, "on_get_joined_communities".to_owned())],
            "the non-community remainder rides the chat seam before the id list resolves"
        );
        let settled = recorded.0.lock().unwrap().take().expect("settled");
        assert_eq!(
            settled
                .expect("the intake scan resolves the value")
                .community_ids,
            vec![7, 9],
            "input order; the intake-failed 8 drops silently"
        );
    }

    #[test]
    fn rename_requires_admin_rights() {
        let mut env = TestEnv::new();
        let mut manager = CommunityManager::new();
        manager.on_get_community(&mut env, &wire_community(7, "My community"), "test");
        let (promise, recorded) = record();
        manager.set_community_name(&mut env, CommunityId::new(7), "new name", promise);
        assert!(
            env.queries
                .iter()
                .any(|q| q == "setCommunityName 7 new name")
        );
        assert_eq!(manager.pending_rename.len(), 1);
        assert!(
            !recorded.0.lock().unwrap().is_some(),
            "the rename promise parks"
        );

        let (promise, recorded) = record();
        manager.set_community_name(&mut env, CommunityId::new(8), "new name", promise);
        assert!(recorded.0.lock().unwrap().is_some());
    }

    #[test]
    fn full_info_builds_the_chat_list() {
        let mut env = TestEnv::new();
        let mut manager = CommunityManager::new();
        manager.on_get_community(&mut env, &wire_community(7, "My community"), "test");
        manager.on_get_community_full(
            &mut env,
            &telegram_api::communityFull {
                flags: 0,
                id: 7,
                about: "about".to_string(),
                chat_photo: None,
                linked_peers: vec![wire_peer()],
                admins_count: 2,
                kicked_count: 1,
                peer_link_requests_pending: 0,
            },
        );
        let info = manager.get_community_full_info_object(&mut env, CommunityId::new(7));
        assert!(info.is_some());
        let info = info.unwrap();
        assert_eq!(info.administrator_count, 2);
        assert_eq!(info.banned_count, 1);
        assert_eq!(info.chats.len(), 1);
        assert_eq!(info.chats[0].chat_id, 3);
    }

    struct Recorded(std::sync::Arc<std::sync::Mutex<Option<std::result::Result<(), Status>>>>);

    struct Recording(std::sync::Arc<std::sync::Mutex<Option<std::result::Result<(), Status>>>>);

    impl tdutils::promise::PromiseInterface<()> for Recording {
        fn set_value(&mut self, _value: ()) {
            *self.0.lock().unwrap() = Some(Ok(()));
        }

        fn set_error(&mut self, error: Status) {
            *self.0.lock().unwrap() = Some(Err(error));
        }
    }

    fn record() -> (Promise<()>, Recorded) {
        let recorded = std::sync::Arc::new(std::sync::Mutex::new(None));
        (
            Promise::from_interface(Recording(recorded.clone())),
            Recorded(recorded),
        )
    }

    struct RecordedId(std::sync::Arc<std::sync::Mutex<Option<td_api::CommunityId>>>);

    struct RecordingId(std::sync::Arc<std::sync::Mutex<Option<td_api::CommunityId>>>);

    impl tdutils::promise::PromiseInterface<td_api::CommunityId> for RecordingId {
        fn set_value(&mut self, value: td_api::CommunityId) {
            *self.0.lock().unwrap() = Some(value);
        }

        fn set_error(&mut self, _error: Status) {}
    }

    fn record_id() -> (Promise<td_api::CommunityId>, RecordedId) {
        let recorded = std::sync::Arc::new(std::sync::Mutex::new(None));
        (
            Promise::from_interface(RecordingId(recorded.clone())),
            RecordedId(recorded),
        )
    }

    struct RecordedMemberChats(
        std::sync::Arc<
            std::sync::Mutex<Option<std::result::Result<td_api::CommunityMemberChats, Status>>>,
        >,
    );

    struct RecordingMemberChats(
        std::sync::Arc<
            std::sync::Mutex<Option<std::result::Result<td_api::CommunityMemberChats, Status>>>,
        >,
    );

    impl tdutils::promise::PromiseInterface<td_api::CommunityMemberChats> for RecordingMemberChats {
        fn set_value(&mut self, value: td_api::CommunityMemberChats) {
            *self.0.lock().unwrap() = Some(Ok(value));
        }

        fn set_error(&mut self, error: Status) {
            *self.0.lock().unwrap() = Some(Err(error));
        }
    }

    struct RecordedPage(
        std::sync::Arc<
            std::sync::Mutex<Option<std::result::Result<td_api::CommunityAddChatRequests, Status>>>,
        >,
    );

    struct RecordingPage(
        std::sync::Arc<
            std::sync::Mutex<Option<std::result::Result<td_api::CommunityAddChatRequests, Status>>>,
        >,
    );

    impl tdutils::promise::PromiseInterface<td_api::CommunityAddChatRequests> for RecordingPage {
        fn set_value(&mut self, value: td_api::CommunityAddChatRequests) {
            *self.0.lock().unwrap() = Some(Ok(value));
        }

        fn set_error(&mut self, error: Status) {
            *self.0.lock().unwrap() = Some(Err(error));
        }
    }

    fn record_page() -> (Promise<td_api::CommunityAddChatRequests>, RecordedPage) {
        let recorded = std::sync::Arc::new(std::sync::Mutex::new(None));
        (
            Promise::from_interface(RecordingPage(recorded.clone())),
            RecordedPage(recorded),
        )
    }

    struct RecordedCommunities(
        std::sync::Arc<std::sync::Mutex<Option<std::result::Result<td_api::Communities, Status>>>>,
    );

    struct RecordingCommunities(
        std::sync::Arc<std::sync::Mutex<Option<std::result::Result<td_api::Communities, Status>>>>,
    );

    impl tdutils::promise::PromiseInterface<td_api::Communities> for RecordingCommunities {
        fn set_value(&mut self, value: td_api::Communities) {
            *self.0.lock().unwrap() = Some(Ok(value));
        }

        fn set_error(&mut self, error: Status) {
            *self.0.lock().unwrap() = Some(Err(error));
        }
    }

    fn record_member_chats() -> (Promise<td_api::CommunityMemberChats>, RecordedMemberChats) {
        let recorded = std::sync::Arc::new(std::sync::Mutex::new(None));
        (
            Promise::from_interface(RecordingMemberChats(recorded.clone())),
            RecordedMemberChats(recorded),
        )
    }

    /// The settled-error message of a recorded promise (the band asserts
    /// share).
    fn recorded_error_message(recorded: &Recorded) -> String {
        recorded
            .0
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .as_ref()
            .unwrap_err()
            .message()
            .to_owned()
    }

    /// The [`recorded_error_message`] form over the member-chats recorder.
    fn recorded_member_chats_error_message(recorded: &RecordedMemberChats) -> String {
        recorded
            .0
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .as_ref()
            .unwrap_err()
            .message()
            .to_owned()
    }

    fn record_communities() -> (Promise<td_api::Communities>, RecordedCommunities) {
        let recorded = std::sync::Arc::new(std::sync::Mutex::new(None));
        (
            Promise::from_interface(RecordingCommunities(recorded.clone())),
            RecordedCommunities(recorded),
        )
    }

    /// A non-creator member wire community (on_get_community derives the
    /// member status when creator/left/admin_rights are all unset).
    fn wire_member_community(id: i64) -> telegram_api::community {
        telegram_api::community {
            creator: false,
            left: false,
            admin_rights: None,
            ..wire_community(id, "My community")
        }
    }

    #[test]
    fn photo_gates_mirror_the_manager() {
        // CommunityManager.cpp:1170-1183 (fb05f53c2): not found → 400,
        // a non-admin member → "Have not enough rights".
        let mut env = TestEnv::new();
        let mut manager = CommunityManager::new();
        manager.on_get_community(&mut env, &wire_community(7, "c"), "test");
        assert!(
            manager
                .set_community_photo_gates(CommunityId::new(7))
                .is_ok()
        );
        assert_eq!(
            manager
                .set_community_photo_gates(CommunityId::new(8))
                .unwrap_err()
                .message(),
            "Community not found"
        );

        manager.on_get_community(&mut env, &wire_member_community(9), "test");
        assert_eq!(
            manager
                .set_community_photo_gates(CommunityId::new(9))
                .unwrap_err()
                .message(),
            "Have not enough rights"
        );
    }

    #[test]
    fn permissions_and_delete_gates() {
        // CommunityManager.cpp:1219-1236 (d55f0a694) and 1268-1278
        // (f5f7fe5fd): the null-permissions band, the restrict/creator
        // gates, and the parked query sends.
        let mut env = TestEnv::new();
        let mut manager = CommunityManager::new();
        manager.on_get_community(&mut env, &wire_community(7, "c"), "test");

        let (promise, recorded) = record();
        manager.set_community_permissions(&mut env, CommunityId::new(7), None, promise);
        assert_eq!(
            recorded
                .0
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .as_ref()
                .unwrap_err()
                .message(),
            "New permissions must be non-empty"
        );

        let (promise, recorded) = record();
        manager.set_community_permissions(&mut env, CommunityId::new(8), None, promise);
        assert_eq!(
            recorded
                .0
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .as_ref()
                .unwrap_err()
                .message(),
            "New permissions must be non-empty"
        );

        let (promise, recorded) = record();
        manager.set_community_permissions(
            &mut env,
            CommunityId::new(8),
            Some(&td_api::CommunityPermissions {
                can_edit_chat_list: true,
            }),
            promise,
        );
        assert_eq!(
            recorded
                .0
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .as_ref()
                .unwrap_err()
                .message(),
            "Community not found"
        );

        let (promise, _recorded) = record();
        manager.set_community_permissions(
            &mut env,
            CommunityId::new(7),
            Some(&td_api::CommunityPermissions {
                can_edit_chat_list: true,
            }),
            promise,
        );
        assert!(
            env.queries
                .iter()
                .any(|q| q.starts_with("setCommunityPermissions 7"))
        );
        assert_eq!(manager.pending_permissions.len(), 1);

        let (promise, recorded) = record();
        manager.delete_community(&mut env, CommunityId::new(7), promise);
        assert!(env.queries.iter().any(|q| q == "deleteCommunity 7"));
        assert_eq!(manager.pending_delete.len(), 1);
        assert!(!recorded.0.lock().unwrap().is_some(), "the promise parks");

        manager.on_get_community(&mut env, &wire_member_community(9), "test");
        let (promise, recorded) = record();
        manager.delete_community(&mut env, CommunityId::new(9), promise);
        assert_eq!(
            recorded
                .0
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .as_ref()
                .unwrap_err()
                .message(),
            "Have not enough rights"
        );
    }

    #[test]
    fn input_peer_builders() {
        // CommunityManager.cpp:1373-1398 (1118a50df): the cached access
        // hash rides, unknown communities answer None unless a bot asks
        // for a valid id.
        let mut env = TestEnv::new();
        let mut manager = CommunityManager::new();
        manager.on_get_community(&mut env, &wire_community(7, "c"), "test");
        let input_channel = manager
            .get_input_community(&env, CommunityId::new(7))
            .unwrap();
        match input_channel {
            telegram_api::InputChannel::inputChannel(c) => {
                assert_eq!(c.channel_id, 7);
                assert_eq!(c.access_hash, 5);
            }
            other => panic!("unexpected input channel {other:?}"),
        }
        let input_peer = manager.get_input_peer(&env, CommunityId::new(7)).unwrap();
        match input_peer {
            telegram_api::InputPeer::inputPeerChannel(p) => {
                assert_eq!(p.channel_id, 7);
                assert_eq!(p.access_hash, 5);
            }
            other => panic!("unexpected input peer {other:?}"),
        }
        assert!(
            manager
                .get_input_community(&env, CommunityId::new(8))
                .is_none()
        );
        assert!(manager.get_input_peer(&env, CommunityId::new(0)).is_none());
    }

    /// A synchronized settings form with an explicit mute.
    fn muted_settings(mute_until: i32) -> DialogNotificationSettings {
        DialogNotificationSettings {
            is_synchronized: true,
            use_default_mute_until: false,
            mute_until,
            ..DialogNotificationSettings::default()
        }
    }

    fn wire_community_full(id: i64) -> telegram_api::communityFull {
        telegram_api::communityFull {
            flags: 0,
            id,
            about: "about".to_string(),
            chat_photo: None,
            linked_peers: vec![wire_peer()],
            admins_count: 2,
            kicked_count: 1,
            peer_link_requests_pending: 0,
        }
    }

    fn wire_peer_notify_settings(mute_until: i32) -> telegram_api::PeerNotifySettings {
        telegram_api::PeerNotifySettings {
            // the mute_until flag (peerNotifySettings bit 2)
            flags: 1 << 2,
            show_previews: true,
            silent: false,
            mute_until,
            ios_sound: None,
            android_sound: None,
            other_sound: None,
            stories_muted: false,
            stories_hide_sender: false,
            stories_ios_sound: None,
            stories_android_sound: None,
            stories_other_sound: None,
        }
    }

    #[test]
    fn need_update_community_dialogs_state_trio() {
        // c9ed2c390: the state trio (mute/pinned/mention) with -1 for the
        // unsynchronized and use_default "unknown" states.
        let unsync = DialogNotificationSettings::default();
        let synced_default = DialogNotificationSettings {
            is_synchronized: true,
            ..DialogNotificationSettings::default()
        };
        let muted = muted_settings(500);

        // unsync and the all-defaults synchronized state share the
        // (-1, -1, -1) trio — no change
        assert!(!CommunityManager::need_update_community_dialogs(
            &unsync,
            &synced_default
        ));
        // the same synchronized defaults are unchanged
        assert!(!CommunityManager::need_update_community_dialogs(
            &synced_default,
            &synced_default.clone()
        ));
        // unsync vs a muted state changed (the -1 triple vs a real one)
        assert!(CommunityManager::need_update_community_dialogs(
            &unsync, &muted
        ));
        // a mute flips the state
        assert!(CommunityManager::need_update_community_dialogs(
            &synced_default,
            &muted
        ));
        // two different non-zero mutes read as the same "muted" state
        let other_mute = muted_settings(600);
        assert!(!CommunityManager::need_update_community_dialogs(
            &muted,
            &other_mute
        ));
        // use_default reads as the unknown -1
        let use_default = DialogNotificationSettings {
            is_synchronized: true,
            mute_until: 500,
            ..DialogNotificationSettings::default()
        };
        assert!(CommunityManager::need_update_community_dialogs(
            &use_default,
            &muted
        ));
    }

    #[test]
    fn update_postpones_until_the_full_arrives() {
        let mut env = TestEnv::new();
        let mut manager = CommunityManager::new();
        manager.on_get_community(&mut env, &wire_community(7, "c"), "test");

        // the full is missing: the change parks + the full reload rides
        // the seam once
        manager.update_community_notification_settings(
            &mut env,
            CommunityId::new(7),
            muted_settings(200),
        );
        assert_eq!(env.reloaded_fulls, vec![7]);
        assert_eq!(
            manager
                .communities
                .get(&7)
                .unwrap()
                .notification_settings
                .mute_until,
            0,
            "the settings stay parked, not applied"
        );
        // a second parked change replaces the first without a second reload
        manager.update_community_notification_settings(
            &mut env,
            CommunityId::new(7),
            muted_settings(300),
        );
        assert_eq!(env.reloaded_fulls, vec![7]);
        assert!(env.fanouts.is_empty());

        // the full arrives: the parked settings apply + the fan-out walks
        // the community chats
        manager.on_get_community_full(&mut env, &wire_community_full(7));
        assert_eq!(
            manager
                .communities
                .get(&7)
                .unwrap()
                .notification_settings
                .mute_until,
            300
        );
        assert_eq!(env.fanouts.len(), 1);
        assert_eq!(env.fanouts[0].0.len(), 1);
        assert_eq!(env.fanouts[0].1, 0, "the old default mute");
        assert_eq!(env.fanouts[0].2, 300, "the new parked mute");
        // the parked map drained
        assert!(manager.pending_community_notification_settings.is_empty());
    }

    #[test]
    fn update_with_full_present_applies_immediately() {
        let mut env = TestEnv::new();
        let mut manager = CommunityManager::new();
        manager.on_get_community(&mut env, &wire_community(7, "c"), "test");
        manager.on_get_community_full(&mut env, &wire_community_full(7));
        env.queries.clear();

        manager.update_community_notification_settings(
            &mut env,
            CommunityId::new(7),
            muted_settings(200),
        );
        assert_eq!(
            manager
                .communities
                .get(&7)
                .unwrap()
                .notification_settings
                .mute_until,
            200
        );
        assert_eq!(env.fanouts.len(), 1);
        assert!(manager.pending_community_notification_settings.is_empty());
        // the unmute deadline is armed
        assert!(manager.next_deadline().is_some());
    }

    #[test]
    fn on_update_community_notify_settings_applies_the_wire_form() {
        let mut env = TestEnv::new();
        let mut manager = CommunityManager::new();
        manager.on_get_community(&mut env, &wire_community(7, "c"), "test");
        manager.on_get_community_full(&mut env, &wire_community_full(7));
        env.queries.clear();
        env.updates.clear();

        // a null settings object converts to the unsynchronized default
        // and is dropped
        manager.on_update_community_notify_settings(&mut env, CommunityId::new(7), None, "test");
        assert!(env.updates.is_empty());
        assert_eq!(
            manager
                .communities
                .get(&7)
                .unwrap()
                .notification_settings
                .mute_until,
            0
        );

        // a synchronized wire form applies to the community + fires the
        // full update; an expired wire mute clears
        let wire = wire_peer_notify_settings(90);
        manager.on_update_community_notify_settings(
            &mut env,
            CommunityId::new(7),
            Some(&wire),
            "test",
        );
        assert_eq!(
            manager
                .communities
                .get(&7)
                .unwrap()
                .notification_settings
                .mute_until,
            0,
            "the expired wire mute cleared"
        );
        assert!(
            !manager
                .communities
                .get(&7)
                .unwrap()
                .notification_settings
                .use_default_mute_until
        );

        let wire = wire_peer_notify_settings(200);
        env.updates.clear();
        manager.on_update_community_notify_settings(
            &mut env,
            CommunityId::new(7),
            Some(&wire),
            "test",
        );
        assert_eq!(
            manager
                .communities
                .get(&7)
                .unwrap()
                .notification_settings
                .mute_until,
            200
        );
        // the full half fired updateCommunityFullInfo
        assert!(matches!(
            env.updates.last(),
            Some(td_api::Update::updateCommunityFullInfo(_))
        ));
    }

    #[test]
    fn set_community_notification_settings_bands() {
        let mut env = TestEnv::new();
        let mut manager = CommunityManager::new();
        manager.on_get_community(&mut env, &wire_community(7, "c"), "test");

        // the missing-community and missing-full 400s
        let (promise, recorded) = record();
        manager.set_community_notification_settings(&mut env, CommunityId::new(8), None, promise);
        assert_eq!(
            recorded
                .0
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .clone()
                .unwrap_err()
                .message(),
            "Community not found"
        );
        let (promise, recorded) = record();
        manager.set_community_notification_settings(&mut env, CommunityId::new(7), None, promise);
        assert_eq!(
            recorded
                .0
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .clone()
                .unwrap_err()
                .message(),
            "Full information about the community must be received first"
        );
        // the null-settings 400
        manager.on_get_community_full(&mut env, &wire_community_full(7));
        let (promise, recorded) = record();
        manager.set_community_notification_settings(&mut env, CommunityId::new(7), None, promise);
        assert_eq!(
            recorded
                .0
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .clone()
                .unwrap_err()
                .message(),
            "New notification settings must be non-empty"
        );

        // the success path: both halves update, the query parks the
        // promise
        let request = td_api::ChatNotificationSettings {
            use_default_mute_for: false,
            mute_for: 100,
            ..default_chat_notification_settings_request()
        };
        let (promise, recorded) = record();
        manager.set_community_notification_settings(
            &mut env,
            CommunityId::new(7),
            Some(&request),
            promise,
        );
        assert!(
            recorded.0.lock().unwrap().is_none(),
            "the promise parks at the query"
        );
        assert_eq!(manager.pending_set_notification_settings.len(), 1);
        assert_eq!(
            manager
                .communities
                .get(&7)
                .unwrap()
                .notification_settings
                .mute_until,
            200,
            "mute_for 100 lifts against unix_time 100"
        );
        assert!(
            env.queries
                .iter()
                .any(|query| query == "updateNotifySettings 7")
        );

        // a request with no server-visible change settles immediately
        let same_again = td_api::ChatNotificationSettings {
            use_default_mute_for: false,
            mute_for: 100,
            ..default_chat_notification_settings_request()
        };
        let (promise, recorded) = record();
        manager.set_community_notification_settings(
            &mut env,
            CommunityId::new(7),
            Some(&same_again),
            promise,
        );
        assert!(recorded.0.lock().unwrap().as_ref().unwrap().is_ok());
        assert_eq!(manager.pending_set_notification_settings.len(), 1);
    }

    fn default_chat_notification_settings_request() -> td_api::ChatNotificationSettings {
        td_api::ChatNotificationSettings {
            use_default_mute_for: true,
            mute_for: 0,
            use_default_sound: true,
            sound_id: 0,
            use_default_show_preview: true,
            show_preview: true,
            use_default_mute_stories: true,
            mute_stories: false,
            use_default_story_sound: true,
            story_sound_id: 0,
            use_default_show_story_poster: true,
            show_story_poster: true,
            use_default_disable_pinned_message_notifications: true,
            disable_pinned_message_notifications: false,
            use_default_disable_mention_notifications: true,
            disable_mention_notifications: false,
        }
    }

    #[test]
    fn community_unmute_timeout_bands() {
        let mut env = TestEnv::new();
        let mut manager = CommunityManager::new();
        manager.on_get_community(&mut env, &wire_community(7, "c"), "test");
        manager.on_get_community_full(&mut env, &wire_community_full(7));
        env.queries.clear();

        // a future mute arms the deadline at now + (mute_until -
        // unix_time + 1)
        manager.update_community_notification_settings(
            &mut env,
            CommunityId::new(7),
            muted_settings(200),
        );
        assert!((manager.next_deadline().unwrap() - 201.0).abs() < 1e-9);
        manager.tick_timeouts(&mut env);
        assert_eq!(
            manager
                .communities
                .get(&7)
                .unwrap()
                .notification_settings
                .mute_until,
            200,
            "not yet due"
        );

        // past the deadline (and the wall clock): the unmute clears the
        // mute through the full update path
        env.now = 302.0;
        env.unix_time = 300;
        manager.tick_timeouts(&mut env);
        assert_eq!(
            manager
                .communities
                .get(&7)
                .unwrap()
                .notification_settings
                .mute_until,
            0
        );
        assert!(manager.next_deadline().is_none());
        assert_eq!(
            env.fanouts.len(),
            2,
            "the mute flip and the clear both fan out"
        );

        // an already-expired mute never arms (fix_mute_until clears it at
        // the update)
        manager.update_community_notification_settings(
            &mut env,
            CommunityId::new(7),
            muted_settings(250),
        );
        assert_eq!(
            manager
                .communities
                .get(&7)
                .unwrap()
                .notification_settings
                .mute_until,
            0,
            "an already-expired arm-time mute never arms"
        );
    }

    #[test]
    fn expired_mute_unmutes_immediately() {
        // d46d51f61: a loaded expired mute (fix_mute_until skipped) fires
        // the timeout at +0 and clears through on_community_unmute
        let mut env = TestEnv::new();
        let mut manager = CommunityManager::new();
        manager.on_get_community(&mut env, &wire_community(7, "c"), "test");
        manager.on_get_community_full(&mut env, &wire_community_full(7));
        env.queries.clear();

        // arm a live mute, then rewind the wall clock so the stored mute
        // reads expired while still scheduled
        manager.update_community_notification_settings(
            &mut env,
            CommunityId::new(7),
            muted_settings(200),
        );
        env.unix_time = 250;
        env.now = 250.0;
        manager.tick_timeouts(&mut env);
        assert_eq!(
            manager
                .communities
                .get(&7)
                .unwrap()
                .notification_settings
                .mute_until,
            0,
            "the fired timeout cleared the expired mute"
        );
        assert_eq!(env.fanouts.last().unwrap().2, 0);
    }

    #[test]
    fn unmute_reschedules_a_not_yet_expired_mute() {
        let mut env = TestEnv::new();
        let mut manager = CommunityManager::new();
        manager.on_get_community(&mut env, &wire_community(7, "c"), "test");
        manager.on_get_community_full(&mut env, &wire_community_full(7));
        env.queries.clear();

        manager.update_community_notification_settings(
            &mut env,
            CommunityId::new(7),
            muted_settings(200),
        );
        // the deadline passes but the wall clock hasn't
        env.now = 150.0;
        env.unix_time = 90;
        manager.tick_timeouts(&mut env);
        assert_eq!(
            manager
                .communities
                .get(&7)
                .unwrap()
                .notification_settings
                .mute_until,
            200,
            "the unmute waits for the wall clock"
        );
        assert!(manager.next_deadline().is_some(), "rescheduled");
    }

    #[test]
    fn get_community_notification_settings_dedups_the_query() {
        let mut env = TestEnv::new();
        let mut manager = CommunityManager::new();
        manager.on_get_community(&mut env, &wire_community(7, "c"), "test");

        // unsynchronized: the read answers None and sends the query once
        assert!(
            manager
                .get_community_notification_settings(&mut env, CommunityId::new(7))
                .is_none()
        );
        assert!(env.queries.iter().any(|q| q == "getNotifySettings 7"));
        let query_count = env
            .queries
            .iter()
            .filter(|q| *q == "getNotifySettings 7")
            .count();
        assert_eq!(query_count, 1);
        assert!(
            manager
                .get_community_notification_settings(&mut env, CommunityId::new(7))
                .is_none()
        );
        let query_count = env
            .queries
            .iter()
            .filter(|q| *q == "getNotifySettings 7")
            .count();
        assert_eq!(query_count, 1, "the in-flight query dedups");

        // 09634ee46: the failed query erases the entry so the next read
        // re-sends
        manager.on_get_community_notification_settings_error(CommunityId::new(7));
        assert!(
            manager
                .get_community_notification_settings(&mut env, CommunityId::new(7))
                .is_none()
        );
        let query_count = env
            .queries
            .iter()
            .filter(|q| *q == "getNotifySettings 7")
            .count();
        assert_eq!(query_count, 2);

        // a synchronized community answers the settings
        manager
            .communities
            .get_mut(&7)
            .unwrap()
            .notification_settings
            .is_synchronized = true;
        let settings = manager
            .get_community_notification_settings(&mut env, CommunityId::new(7))
            .unwrap();
        assert!(settings.is_synchronized);

        // an unknown community answers None
        assert!(
            manager
                .get_community_notification_settings(&mut env, CommunityId::new(8))
                .is_none()
        );
    }

    #[test]
    fn reset_all_resets_every_community() {
        let mut env = TestEnv::new();
        let mut manager = CommunityManager::new();
        manager.on_get_community(&mut env, &wire_community(7, "c"), "test");
        manager.on_get_community(&mut env, &wire_community(8, "c2"), "test");
        manager.on_get_community_full(&mut env, &wire_community_full(7));
        manager.update_community_notification_settings(
            &mut env,
            CommunityId::new(7),
            muted_settings(200),
        );
        manager.update_community_notification_settings(
            &mut env,
            CommunityId::new(8),
            muted_settings(300),
        );
        assert!(manager.next_deadline().is_some());

        // 8d2a35e05: the reset clears both halves of every community
        manager.reset_all_community_notification_settings(&mut env);
        for id in [7, 8] {
            let settings = &manager.communities.get(&id).unwrap().notification_settings;
            assert!(settings.is_synchronized);
            assert!(settings.use_default_mute_until);
            assert_eq!(settings.mute_until, 0);
        }
        assert!(
            manager.communities_full[&7]
                .notification_settings
                .use_default_mute_until
        );
        assert!(manager.next_deadline().is_none(), "the unmutes cancel");
    }

    #[test]
    fn full_info_object_carries_notification_settings() {
        let mut env = TestEnv::new();
        let mut manager = CommunityManager::new();
        manager.on_get_community(&mut env, &wire_community(7, "c"), "test");
        manager.on_get_community_full(&mut env, &wire_community_full(7));

        // the wire update refreshes the full's half, which the object
        // converts (mute_for = mute_until - now)
        let wire = wire_peer_notify_settings(150);
        manager.on_update_community_notify_settings(
            &mut env,
            CommunityId::new(7),
            Some(&wire),
            "test",
        );
        let info = manager
            .get_community_full_info_object(&mut env, CommunityId::new(7))
            .unwrap();
        let settings = info.notification_settings.unwrap();
        assert!(!settings.use_default_mute_for);
        assert_eq!(settings.mute_for, 50);
    }

    // ---- the change-settle (on_change_community_dialogs) ----

    #[test]
    fn change_settle_settles_now_without_a_cached_full() {
        // CommunityManager.cpp:2051-2053 — `get_community_full_const ==
        // nullptr` settles the request immediately, with a cached
        // community and for a fully unknown one alike.
        let mut env = TestEnv::new();
        let mut manager = CommunityManager::new();
        manager.on_get_community(&mut env, &wire_community(7, "c"), "test");

        let (promise, recorded) = record();
        manager.on_change_community_dialogs(&mut env, CommunityId::new(7), promise);
        assert!(
            matches!(recorded.0.lock().unwrap().take(), Some(Ok(()))),
            "the request settles ok now"
        );
        assert!(env.reloaded_fulls.is_empty(), "no reload fires");

        let (promise, recorded) = record();
        manager.on_change_community_dialogs(&mut env, CommunityId::new(8), promise);
        assert!(matches!(recorded.0.lock().unwrap().take(), Some(Ok(()))));
        assert!(env.reloaded_fulls.is_empty());
    }

    #[test]
    fn change_settle_reloads_and_settles_when_the_reload_answers_ok() {
        let mut env = TestEnv::new();
        let mut manager = CommunityManager::new();
        manager.on_get_community(&mut env, &wire_community(7, "c"), "test");
        manager.on_get_community_full(&mut env, &wire_community_full(7));

        let (promise, recorded) = record();
        manager.on_change_community_dialogs(&mut env, CommunityId::new(7), promise);
        assert_eq!(env.reloaded_fulls, vec![7], "the full reloads");
        assert_eq!(manager.pending_change_dialogs.len(), 1);
        assert!(
            !recorded.0.lock().unwrap().is_some(),
            "the promise parks on the reload"
        );

        // the reload answers with the communityFull — the intake tail
        // drains the parked settle
        manager.on_get_community_full(&mut env, &wire_community_full(7));
        assert!(
            matches!(recorded.0.lock().unwrap().take(), Some(Ok(()))),
            "the request settles ok on the reload"
        );
        assert_eq!(env.reloaded_fulls, vec![7], "the intake itself sends none");
        assert!(manager.pending_change_dialogs.is_empty());
    }

    #[test]
    fn change_settle_settles_ok_on_the_reload_error_too() {
        // CommunityManager.cpp:2054-2057 — the settle lambda ignores the
        // reload Result; the failed branch runs the same reload tail the
        // answer arm's failed_reload body runs.
        let mut env = TestEnv::new();
        let mut manager = CommunityManager::new();
        manager.on_get_community(&mut env, &wire_community(7, "c"), "test");
        manager.on_get_community_full(&mut env, &wire_community_full(7));

        let (promise, recorded) = record();
        manager.on_change_community_dialogs(&mut env, CommunityId::new(7), promise);
        assert_eq!(env.reloaded_fulls, vec![7]);

        manager.on_reload_community_full_for_notification_settings(&mut env, CommunityId::new(7));
        assert!(
            matches!(recorded.0.lock().unwrap().take(), Some(Ok(()))),
            "the reload error never propagates to the request"
        );
        assert!(manager.pending_change_dialogs.is_empty());
    }

    // ---- the add/remove community dialog link toggles (#131) ----

    fn channel_dialog(id: i64) -> DialogId {
        DialogId::from_channel_id(crate::chat_manager::ChannelId::new(id))
    }

    fn group_dialog(id: i64) -> DialogId {
        DialogId::from_chat_id(crate::chat_manager::ChatId::new(id))
    }

    fn user_dialog(id: i64) -> DialogId {
        DialogId::from_user_id(crate::user_manager::UserId::new(id))
    }

    fn secret_dialog(id: i64) -> DialogId {
        DialogId::from_secret_chat_id(crate::user_manager::SecretChatId::new(
            id.try_into().unwrap(),
        ))
    }

    #[test]
    fn add_community_dialog_gate_bands() {
        // CommunityManager.cpp:2005-2017 (1747ecde1 + the da0e19cf3
        // owner gate): the known-community gate, the Read access check,
        // the owner test, then the visible-band toggle send with the
        // promise parked at the toggle lot.
        let mut env = TestEnv::new();
        let mut manager = CommunityManager::new();
        manager.on_get_community(&mut env, &wire_community(7, "c"), "test");

        // the known-community gate (have_community)
        let (promise, recorded) = record();
        manager.add_community_dialog(
            &mut env,
            CommunityId::new(8),
            channel_dialog(3),
            false,
            promise,
        );
        assert_eq!(
            recorded
                .0
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .as_ref()
                .unwrap_err()
                .message(),
            "Community not found"
        );

        // the Read access check surfaces its error
        env.failing_access = vec![channel_dialog(3)];
        let (promise, recorded) = record();
        manager.add_community_dialog(
            &mut env,
            CommunityId::new(7),
            channel_dialog(3),
            false,
            promise,
        );
        assert_eq!(
            recorded
                .0
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .as_ref()
                .unwrap_err()
                .message(),
            "Can't access the chat"
        );
        env.failing_access.clear();

        // the owner test: a non-creator channel is not the owner's
        let (promise, recorded) = record();
        manager.add_community_dialog(
            &mut env,
            CommunityId::new(7),
            channel_dialog(3),
            false,
            promise,
        );
        assert_eq!(
            recorded
                .0
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .as_ref()
                .unwrap_err()
                .message(),
            "Have not enough rights"
        );
        assert!(env.queries.is_empty(), "no toggle leaves the gate band");

        // the owner's channel sends the visible band and parks the
        // promise at the toggle lot
        env.creator_dialogs = vec![channel_dialog(3)];
        let (promise, recorded) = record();
        manager.add_community_dialog(
            &mut env,
            CommunityId::new(7),
            channel_dialog(3),
            false,
            promise,
        );
        assert_eq!(
            env.queries,
            vec!["togglePeerLink 7 DialogId(-1000000000003) deleted=false hidden=false"]
        );
        assert_eq!(manager.pending_toggle_peer_link.len(), 1);
        assert!(
            !recorded.0.lock().unwrap().is_some(),
            "the promise parks at the toggle query"
        );
    }

    #[test]
    fn is_dialog_owner_bands() {
        // CommunityManager.cpp:2019-2034 (f027a2d06): a user dialog is
        // the owner's iff the bot is known and can be edited, a basic
        // group or a channel iff the user created it, a secret chat or
        // the empty dialog never.
        let mut env = TestEnv::new();
        let manager = CommunityManager::new();

        assert!(!manager.is_dialog_owner(&env, user_dialog(5)));
        env.editable_bots = vec![5];
        assert!(manager.is_dialog_owner(&env, user_dialog(5)));

        assert!(!manager.is_dialog_owner(&env, group_dialog(9)));
        assert!(!manager.is_dialog_owner(&env, channel_dialog(3)));
        env.creator_dialogs = vec![group_dialog(9), channel_dialog(3)];
        assert!(manager.is_dialog_owner(&env, group_dialog(9)));
        assert!(manager.is_dialog_owner(&env, channel_dialog(3)));

        assert!(!manager.is_dialog_owner(&env, secret_dialog(4)));
        assert!(!manager.is_dialog_owner(&env, DialogId::new(0)));
    }

    #[test]
    fn add_community_dialog_hidden_band() {
        // CommunityManager.cpp:366-368 — the hidden add rides the seam
        // with its own band (the wire flags are the env sender's).
        let mut env = TestEnv::new();
        let mut manager = CommunityManager::new();
        manager.on_get_community(&mut env, &wire_community(7, "c"), "test");
        env.editable_bots = vec![5];

        let (promise, _recorded) = record();
        manager.add_community_dialog(&mut env, CommunityId::new(7), user_dialog(5), true, promise);
        assert_eq!(
            env.queries,
            vec!["togglePeerLink 7 DialogId(5) deleted=false hidden=true"]
        );
        assert_eq!(manager.pending_toggle_peer_link.len(), 1);
    }

    /// A linked-peer administrator community (admin_rights bit 19) — the
    /// remove gate's administrator band (DialogParticipant.h:524-526).
    fn wire_linked_peer_admin_community(id: i64) -> telegram_api::community {
        telegram_api::community {
            creator: false,
            admin_rights: Some(telegram_api::ChatAdminRights {
                manage_linked_peers: true,
                ..telegram_api::ChatAdminRights::default()
            }),
            ..wire_community(id, "My community")
        }
    }

    #[test]
    fn remove_community_dialog_gate_bands() {
        // CommunityManager.cpp:2036-2047 (f027a2d06): the get_community
        // gate, the Read access check, the linked-peer-administrator OR
        // owner rights gate, then the deleted-band toggle send.
        let mut env = TestEnv::new();
        let mut manager = CommunityManager::new();
        manager.on_get_community(&mut env, &wire_member_community(7), "test");

        // the get_community gate
        let (promise, recorded) = record();
        manager.remove_community_dialog(&mut env, CommunityId::new(8), channel_dialog(3), promise);
        assert_eq!(
            recorded
                .0
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .as_ref()
                .unwrap_err()
                .message(),
            "Community not found"
        );

        // the Read access check surfaces its error
        env.failing_access = vec![channel_dialog(3)];
        let (promise, recorded) = record();
        manager.remove_community_dialog(&mut env, CommunityId::new(7), channel_dialog(3), promise);
        assert_eq!(
            recorded
                .0
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .as_ref()
                .unwrap_err()
                .message(),
            "Can't access the chat"
        );
        env.failing_access.clear();

        // neither a linked-peer administrator nor the dialog owner
        let (promise, recorded) = record();
        manager.remove_community_dialog(&mut env, CommunityId::new(7), channel_dialog(3), promise);
        assert_eq!(
            recorded
                .0
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .as_ref()
                .unwrap_err()
                .message(),
            "Have not enough rights"
        );
        assert!(env.queries.is_empty(), "no toggle leaves the gate band");

        // the dialog owner removes its own chat (the community status is
        // a plain member's) — the deleted band
        env.creator_dialogs = vec![channel_dialog(3)];
        let (promise, recorded) = record();
        manager.remove_community_dialog(&mut env, CommunityId::new(7), channel_dialog(3), promise);
        assert_eq!(
            env.queries,
            vec!["togglePeerLink 7 DialogId(-1000000000003) deleted=true hidden=false"]
        );
        assert_eq!(manager.pending_toggle_peer_link.len(), 1);
        assert!(!recorded.0.lock().unwrap().is_some(), "the promise parks");

        // a linked-peer administrator removes a chat it does not own
        manager.on_get_community(&mut env, &wire_linked_peer_admin_community(9), "test");
        let (promise, _recorded) = record();
        manager.remove_community_dialog(&mut env, CommunityId::new(9), channel_dialog(4), promise);
        assert_eq!(
            env.queries.last().unwrap(),
            "togglePeerLink 9 DialogId(-1000000000004) deleted=true hidden=false"
        );
        assert_eq!(manager.pending_toggle_peer_link.len(), 2);

        // a creator community carries every administrator right — the
        // gate passes without the dialog ownership
        manager.on_get_community(&mut env, &wire_community(10, "c"), "test");
        let (promise, _recorded) = record();
        manager.remove_community_dialog(&mut env, CommunityId::new(10), channel_dialog(4), promise);
        assert_eq!(
            env.queries.last().unwrap(),
            "togglePeerLink 10 DialogId(-1000000000004) deleted=true hidden=false"
        );
    }

    // ---- the member pair (toggleCommunityMemberIsBanned +
    // getCommunityMemberChats) ----

    #[test]
    fn member_pair_gates_and_wire_bands() {
        // CommunityManager.cpp:2107-2122 (20fc28e23) and 2124-2140
        // (4cd0dae69): the shared gate ladder — the community gate, the
        // restrict-members right, then the Know-peer "Member not found"
        // band (the 400 comes from the input-peer lookup, after the
        // sender→dialog conversion the request half runs) — and the two
        // wire query sends park their promises.
        let mut env = TestEnv::new();
        let mut manager = CommunityManager::new();
        manager.on_get_community(&mut env, &wire_community(7, "c"), "test");
        let member_dialog_id = DialogId::new(5);
        env.known_members = vec![5];

        // the unknown community answers its 400 on both
        let (promise, recorded) = record();
        manager.toggle_community_participant_is_banned(
            &mut env,
            CommunityId::new(8),
            member_dialog_id,
            true,
            promise,
        );
        assert_eq!(recorded_error_message(&recorded), "Community not found");
        let (promise, recorded) = record_member_chats();
        manager.get_community_participant_dialogs(
            &mut env,
            CommunityId::new(8),
            member_dialog_id,
            promise,
        );
        assert_eq!(
            recorded_member_chats_error_message(&recorded),
            "Community not found"
        );

        // a plain member lacks the restrict-members right
        manager.on_get_community(&mut env, &wire_member_community(9), "test");
        let (promise, recorded) = record();
        manager.toggle_community_participant_is_banned(
            &mut env,
            CommunityId::new(9),
            member_dialog_id,
            true,
            promise,
        );
        assert_eq!(recorded_error_message(&recorded), "Have not enough rights");
        let (promise, recorded) = record_member_chats();
        manager.get_community_participant_dialogs(
            &mut env,
            CommunityId::new(9),
            member_dialog_id,
            promise,
        );
        assert_eq!(
            recorded_member_chats_error_message(&recorded),
            "Have not enough rights"
        );

        // the unknown member answers the Know band on both
        env.known_members = Vec::new();
        let (promise, recorded) = record();
        manager.toggle_community_participant_is_banned(
            &mut env,
            CommunityId::new(7),
            member_dialog_id,
            true,
            promise,
        );
        assert_eq!(recorded_error_message(&recorded), "Member not found");
        let (promise, recorded) = record_member_chats();
        manager.get_community_participant_dialogs(
            &mut env,
            CommunityId::new(7),
            member_dialog_id,
            promise,
        );
        assert_eq!(
            recorded_member_chats_error_message(&recorded),
            "Member not found"
        );
        assert!(env.queries.is_empty(), "the bands send nothing");

        // the ban walk: the creator community sends the participant
        // toggle with the unban inversion band and parks the promise
        env.known_members = vec![5];
        let (promise, recorded) = record();
        manager.toggle_community_participant_is_banned(
            &mut env,
            CommunityId::new(7),
            member_dialog_id,
            true,
            promise,
        );
        assert!(
            env.queries
                .iter()
                .any(|q| q == "toggleParticipantBanned 7 user 5 ban")
        );
        let (promise, unban_recorded) = record();
        manager.toggle_community_participant_is_banned(
            &mut env,
            CommunityId::new(7),
            member_dialog_id,
            false,
            promise,
        );
        assert!(
            env.queries
                .iter()
                .any(|q| q == "toggleParticipantBanned 7 user 5 unban")
        );
        assert_eq!(manager.pending_ban_toggles.len(), 2);
        assert!(
            !recorded.0.lock().unwrap().is_some(),
            "the ban promise parks at the query"
        );
        assert!(!unban_recorded.0.lock().unwrap().is_some());

        // the member-chats walk sends the joined-chats query
        let (promise, recorded) = record_member_chats();
        manager.get_community_participant_dialogs(
            &mut env,
            CommunityId::new(7),
            member_dialog_id,
            promise,
        );
        assert!(
            env.queries
                .iter()
                .any(|q| q == "getParticipantJoinedChats 7 user 5")
        );
        assert_eq!(manager.pending_member_chats.len(), 1);
        assert!(
            recorded.0.lock().unwrap().is_none(),
            "the member-chats promise parks at the query"
        );
    }

    #[test]
    fn toggle_collapse_gates_and_parks() {
        // CommunityManager.cpp:2098-2105: the known-community gate only
        // (no rights gate) — a plain member toggles fine; the query send
        // parks the promise on the FIFO lot.
        let mut env = TestEnv::new();
        let mut manager = CommunityManager::new();
        let (promise, recorded) = record();
        manager.toggle_community_are_dialogs_collapsed(
            &mut env,
            CommunityId::new(8),
            true,
            promise,
        );
        assert_eq!(
            recorded
                .0
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .as_ref()
                .unwrap_err()
                .message(),
            "Community not found"
        );
        assert!(env.queries.is_empty(), "no query without the community");

        manager.on_get_community(&mut env, &wire_member_community(7), "test");
        let (promise, recorded) = record();
        manager.toggle_community_are_dialogs_collapsed(
            &mut env,
            CommunityId::new(7),
            true,
            promise,
        );
        assert!(
            env.queries
                .iter()
                .any(|q| q == "toggleCommunityCollapsedInDialogs 7 true"),
            "the wire toggle rides the seam"
        );
        assert_eq!(manager.pending_collapse.len(), 1);
        assert!(
            !recorded.0.lock().unwrap().is_some(),
            "the promise parks for the Updates answer"
        );
    }

    #[test]
    fn collapse_flip_quiet_expansion_fans_out() {
        // the collapsed flag diff (CommunityManager.cpp + ea2241d48):
        // the collapse itself is a quiet change tracking (no folder
        // removal), the expansion fires the
        // remove-community-from-dialog_filters fan-out.
        let mut env = TestEnv::new();
        let mut manager = CommunityManager::new();
        manager.on_get_community(&mut env, &wire_community(7, "c"), "test");
        assert!(env.filter_removals.is_empty());

        // collapse: the flag flips, updateCommunity fires, no fan-out
        let mut wire = wire_community(7, "c");
        wire.collapsed_in_dialogs = true;
        manager.on_get_community(&mut env, &wire, "test");
        assert_eq!(env.updates.len(), 2, "the flip emits updateCommunity");
        assert!(matches!(
            env.updates.last(),
            Some(td_api::Update::updateCommunity(update)) if update.community.as_ref().unwrap().are_chats_collapsed
        ));
        assert!(env.filter_removals.is_empty(), "the collapse stays quiet");
        assert!(
            manager.communities.get(&7).unwrap().collapsed_in_dialogs,
            "the flag sticks"
        );

        // the object shape follows the flag (can_edit_chat_list inverts)
        let object = manager.get_community_object(CommunityId::new(7)).unwrap();
        assert!(object.are_chats_collapsed);
        assert!(!object.permissions.as_ref().unwrap().can_edit_chat_list);

        // a same-state re-intake is fully quiet
        env.updates.clear();
        manager.on_get_community(&mut env, &wire, "test");
        assert!(env.updates.is_empty(), "no update without a diff");

        // expansion: the flag flips back and the folder fan-out fires
        manager.on_get_community(&mut env, &wire_community(7, "c"), "test");
        assert_eq!(env.updates.len(), 1, "the expansion emits updateCommunity");
        assert!(matches!(
            env.updates.last(),
            Some(td_api::Update::updateCommunity(update)) if !update.community.as_ref().unwrap().are_chats_collapsed
        ));
        assert_eq!(env.filter_removals, vec![7], "the expansion fans out");
        assert_eq!(env.filter_removals.len(), 1, "exactly one fan-out");
    }

    // ---- the add-chat requests queue (#132) ----

    #[test]
    fn add_chat_requests_validation_bands() {
        // CommunityManager.cpp:2060-2070 — the have_community gate first,
        // then the positive-limit band, then the
        // GetCommunityPeerLinkRequestsQuery send over the input community.
        let mut env = TestEnv::new();
        let mut manager = CommunityManager::new();
        manager.on_get_community(&mut env, &wire_community(7, "c"), "test");

        let (promise, recorded) = record_page();
        manager.get_community_add_dialog_requests(&mut env, CommunityId::new(8), "", 10, promise);
        assert_eq!(
            recorded
                .0
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .unwrap_err()
                .message(),
            "Community not found"
        );

        let (promise, recorded) = record_page();
        manager.get_community_add_dialog_requests(&mut env, CommunityId::new(7), "", 0, promise);
        assert_eq!(
            recorded
                .0
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .unwrap_err()
                .message(),
            "Parameter limit must be positive"
        );

        let (promise, recorded) = record_page();
        manager.get_community_add_dialog_requests(&mut env, CommunityId::new(7), "", -1, promise);
        assert_eq!(
            recorded
                .0
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .unwrap_err()
                .message(),
            "Parameter limit must be positive"
        );

        let (promise, recorded) = record_page();
        manager.get_community_add_dialog_requests(
            &mut env,
            CommunityId::new(7),
            "off",
            10,
            promise,
        );
        assert!(
            env.queries
                .iter()
                .any(|q| q == "getPeerLinkRequests 7 off 10")
        );
        assert_eq!(manager.pending_peer_link_requests.len(), 1);
        assert!(
            !recorded.0.lock().unwrap().is_some(),
            "the page promise parks at the query"
        );
    }

    #[test]
    fn add_chat_requests_page_building() {
        // `GetCommunityPeerLinkRequestsQuery::on_result`
        // (CommunityManager.cpp:411-434): the users + chats intakes run
        // first, the total clamps up to the page size, invalid entries
        // drop with the count decremented, the dialogs force-create, and
        // each page entry carries the hidden inversion
        // (`is_hidden = !visible`) with the next offset passed through.
        let mut env = TestEnv::new();
        let mut manager = CommunityManager::new();
        let (promise, recorded) = record_page();
        manager.on_get_community_peer_link_requests(
            &mut env,
            telegram_api::communities_PeerLinkRequests {
                flags: 1,
                // smaller than the page — clamps up to 3, then the two
                // invalid entries decrement it to 1
                total_count: 0,
                requests: vec![
                    telegram_api::CommunityPeerRequest {
                        flags: 0,
                        visible: false,
                        peer: Some(telegram_api::Peer::peerChannel(telegram_api::peerChannel {
                            channel_id: 3,
                        })),
                        requested_by: 42,
                        date: 77,
                    },
                    telegram_api::CommunityPeerRequest {
                        flags: 0,
                        visible: true,
                        // an invalid dialog id (channel 0) drops the entry
                        peer: Some(telegram_api::Peer::peerChannel(telegram_api::peerChannel {
                            channel_id: 0,
                        })),
                        requested_by: 42,
                        date: 78,
                    },
                    telegram_api::CommunityPeerRequest {
                        flags: 0,
                        visible: true,
                        peer: Some(telegram_api::Peer::peerChannel(telegram_api::peerChannel {
                            channel_id: 4,
                        })),
                        // an invalid user id drops the entry
                        requested_by: 0,
                        date: 79,
                    },
                ],
                next_offset: "next".to_owned(),
                chats: vec![telegram_api::Chat::chatEmpty(telegram_api::chatEmpty {
                    id: 3,
                })],
                users: vec![telegram_api::User::userEmpty(telegram_api::userEmpty {
                    id: 42,
                })],
            },
            promise,
        );
        let page = recorded
            .0
            .lock()
            .unwrap()
            .take()
            .unwrap()
            .expect("the page resolves");
        assert_eq!(
            page.total_count, 1,
            "0 clamps up to 3, the two drops decrement"
        );
        assert_eq!(page.next_offset, "next");
        assert_eq!(page.requests.len(), 1);
        let request = &page.requests[0];
        assert!(request.is_hidden, "the hidden inversion");
        assert_eq!(request.sender_user_id, 42);
        assert_eq!(request.date, 77);
        assert_eq!(request.chat_id, 3, "get_chat_id_object");

        assert_eq!(
            env.intaken_users,
            vec![(1, "GetCommunityPeerLinkRequestsQuery".to_owned())],
            "the users intake runs first"
        );
        assert_eq!(
            env.intaken_chats,
            vec![(1, "GetCommunityPeerLinkRequestsQuery".to_owned())]
        );
        assert_eq!(
            env.forced_dialogs,
            vec![(
                DialogId::from_peer(&telegram_api::Peer::peerChannel(
                    telegram_api::peerChannel { channel_id: 3 }
                )),
                "GetCommunityPeerLinkRequestsQuery".to_owned()
            )],
            "only the valid entry's dialog force-creates"
        );
    }

    #[test]
    fn process_add_chat_request_gates() {
        // CommunityManager.cpp:2072-2084 (2b80bf533) — the get_community
        // gate, the admin-only linked-peer right, the read access on the
        // requested chat, then the single-approval toggle send with
        // reject = !approve.
        let mut env = TestEnv::new();
        let mut manager = CommunityManager::new();
        manager.on_get_community(&mut env, &wire_community(7, "c"), "test");
        manager.on_get_community(&mut env, &wire_member_community(9), "test");

        let (promise, recorded) = record();
        manager.process_community_add_dialog_request(
            &mut env,
            CommunityId::new(8),
            DialogId::new(1),
            true,
            promise,
        );
        assert_eq!(
            recorded
                .0
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .unwrap_err()
                .message(),
            "Community not found"
        );

        let (promise, recorded) = record();
        manager.process_community_add_dialog_request(
            &mut env,
            CommunityId::new(9),
            DialogId::new(1),
            true,
            promise,
        );
        assert_eq!(
            recorded
                .0
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .unwrap_err()
                .message(),
            "Have not enough rights"
        );

        env.dialog_access_error = Some(Status::error(400, "Chat not found"));
        let (promise, recorded) = record();
        manager.process_community_add_dialog_request(
            &mut env,
            CommunityId::new(7),
            DialogId::new(1),
            true,
            promise,
        );
        assert_eq!(
            recorded
                .0
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .unwrap_err()
                .message(),
            "Chat not found"
        );
        env.dialog_access_error = None;

        let (promise, recorded) = record();
        manager.process_community_add_dialog_request(
            &mut env,
            CommunityId::new(7),
            DialogId::new(1),
            false,
            promise,
        );
        assert!(
            env.queries
                .iter()
                .any(|q| q == "togglePeerLinkRequestApproval 7 DialogId(1) reject=true"),
            "the reject band inverts approve"
        );
        assert_eq!(manager.pending_process_request.len(), 1);
        assert!(
            !recorded.0.lock().unwrap().is_some(),
            "the approval promise parks at the query"
        );
    }

    #[test]
    fn process_add_chat_requests_gates() {
        // CommunityManager.cpp:2086-2096 (79a943019) — the same community
        // gates with no per-chat access check, then the all-approvals
        // toggle send with reject = !approve.
        let mut env = TestEnv::new();
        let mut manager = CommunityManager::new();
        manager.on_get_community(&mut env, &wire_community(7, "c"), "test");
        manager.on_get_community(&mut env, &wire_member_community(9), "test");

        let (promise, recorded) = record();
        manager.process_community_add_dialog_requests(&mut env, CommunityId::new(8), true, promise);
        assert_eq!(
            recorded
                .0
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .unwrap_err()
                .message(),
            "Community not found"
        );

        let (promise, recorded) = record();
        manager.process_community_add_dialog_requests(&mut env, CommunityId::new(9), true, promise);
        assert_eq!(
            recorded
                .0
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .unwrap_err()
                .message(),
            "Have not enough rights"
        );

        let (promise, recorded) = record();
        manager.process_community_add_dialog_requests(&mut env, CommunityId::new(7), true, promise);
        assert!(
            env.queries
                .iter()
                .any(|q| q == "toggleAllPeerLinkRequestApproval 7 reject=false"),
            "the reject band inverts approve"
        );
        assert_eq!(manager.pending_process_all_requests.len(), 1);
        assert!(
            !recorded.0.lock().unwrap().is_some(),
            "the approval promise parks at the query"
        );
    }
}
