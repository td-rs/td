//! `td::DialogParticipantManager` (DialogParticipantManager.{h,cpp}) — the
//! membership plane upstream extracted from MessagesManager: administrators,
//! member queries/search, join/leave/add/ban mutations, join requests and the
//! ownership-transfer machine.
//!
//! Slice 1 (the M.10 next-slices queue): every request arm's LOCAL decision
//! plane is C++-exact. Slice 2 (net-drain 35): the member-query families —
//! `GetChannelParticipantQuery`, `GetChannelParticipantsQuery` and
//! `EditChatCreatorQuery` — ride the net plane (`DialogParticipantNetKind`,
//! the `crate::td::DialogParticipantNetHandle`) over the
//! [`DialogParticipantManagerEnv`] send arms, with the promises parked in
//! the manager's FIFO lots (`PendingDialogParticipantQueries`); the rest of
//! the wire seams stay `[blocked: net]`, parking exactly like the C++
//! request without a network.

use crate::chat_manager::DialogId;
use crate::chat_manager::DialogType;
use crate::chat_manager::{
    AdministratorRights, ChannelId, ChannelType, ChatId, DialogAdministrator, DialogParticipant,
    DialogParticipantStatus, RestrictedRights,
};
use crate::dialog_participant_filter::{ChannelParticipantFilter, DialogParticipantFilter};
use crate::misc::get_vector_hash;
use crate::store_parse::{Parser, store_i32};
use crate::td_api;
use crate::user_manager::UserId;
use std::collections::HashMap;
use tdmtproto::telegram_api;
use tdutils::hints::Hints;
use tdutils::promise::Promise;
use tdutils::status::Status;

/// `DialogParticipantManager::ONLINE_MEMBER_COUNT_CACHE_EXPIRE_TIME`
/// (DialogParticipantManager.h:169).
const ONLINE_MEMBER_COUNT_CACHE_EXPIRE_TIME: i32 = 30 * 60;
/// `DialogParticipantManager::ONLINE_MEMBER_COUNT_UPDATE_TIME`
/// (DialogParticipantManager.h:171).
const ONLINE_MEMBER_COUNT_UPDATE_TIME: i32 = 5 * 60;
/// `DialogParticipantManager::CHANNEL_PARTICIPANT_CACHE_TIME`
/// (DialogParticipantManager.h:173).
const CHANNEL_PARTICIPANT_CACHE_TIME: i32 = 1800;
/// `DialogParticipantManager::MAX_GET_CHANNEL_PARTICIPANTS`
/// (DialogParticipantManager.h:175) — the server-side limit.
const MAX_GET_CHANNEL_PARTICIPANTS: i32 = 200;
/// `DialogParticipantManager::MAX_PENDING_JOIN_REQUESTS`
/// (DialogParticipantManager.cpp:1327).
const MAX_PENDING_JOIN_REQUESTS: usize = 3;

/// the `vector<DialogAdministrator>` log-event codec — the C++
/// `log_event_store(vector)` shape: an i32 size, then each element's store.
pub fn store_dialog_administrators(administrators: &[DialogAdministrator], out: &mut Vec<u8>) {
    store_i32(out, administrators.len() as i32);
    for administrator in administrators {
        administrator.store(out);
    }
}

/// the inverse of [`store_dialog_administrators`].
pub fn parse_dialog_administrators(
    parser: &mut Parser<'_>,
) -> Result<Vec<DialogAdministrator>, &'static str> {
    let size = parser.i32_()?;
    if size < 0 {
        return Err("Wrong vector length");
    }
    let mut administrators = Vec::with_capacity(size.min(1024) as usize);
    for _ in 0..size {
        administrators.push(DialogAdministrator::parse_impl(parser)?);
    }
    Ok(administrators)
}

/// `get_dialog_participant_status` (DialogParticipant.cpp:676-712) — the
/// td_api `ChatMemberStatus` decision table; a null status is a plain member.
pub fn get_dialog_participant_status(
    env: &dyn DialogParticipantManagerEnv,
    status: Option<&td_api::ChatMemberStatus>,
    channel_type: ChannelType,
) -> DialogParticipantStatus {
    // `fix_until_date` — restricted for under 30 seconds or over 366 days is
    // restricted forever (DialogParticipant.cpp:681-692)
    let fix_until_date = |until_date: i32| -> i32 {
        if until_date == 0 {
            return 0;
        }
        let unix_time = env.unix_time();
        if until_date < unix_time + 30 || until_date > unix_time + 366 * 86400 {
            return 0;
        }
        until_date
    };

    use td_api::ChatMemberStatus as S;
    match status {
        None => DialogParticipantStatus::member(0, String::new()),
        Some(S::chatMemberStatusCreator(st)) => {
            DialogParticipantStatus::creator(st.is_member, st.is_anonymous, String::new())
        }
        Some(S::chatMemberStatusAdministrator(st)) => {
            // the C++ AdministratorRights(td_api) constructor maps a null
            // rights object to the zero flags
            let rights = st.rights.as_ref();
            DialogParticipantStatus::administrator(
                AdministratorRights::from_bools(
                    rights.is_some_and(|rights| rights.is_anonymous),
                    rights.is_some_and(|rights| rights.can_manage_chat),
                    rights.is_some_and(|rights| rights.can_change_info),
                    rights.is_some_and(|rights| rights.can_post_messages),
                    rights.is_some_and(|rights| rights.can_edit_messages),
                    rights.is_some_and(|rights| rights.can_delete_messages),
                    rights.is_some_and(|rights| rights.can_invite_users),
                    rights.is_some_and(|rights| rights.can_restrict_members),
                    rights.is_some_and(|rights| rights.can_pin_messages),
                    rights.is_some_and(|rights| rights.can_manage_topics),
                    rights.is_some_and(|rights| rights.can_promote_members),
                    rights.is_some_and(|rights| rights.can_manage_video_chats),
                    rights.is_some_and(|rights| rights.can_post_stories),
                    rights.is_some_and(|rights| rights.can_edit_stories),
                    rights.is_some_and(|rights| rights.can_delete_stories),
                    rights.is_some_and(|rights| rights.can_manage_direct_messages),
                    rights.is_some_and(|rights| rights.can_manage_tags),
                    false,
                    rights.is_some_and(|rights| rights.can_send_welcome_messages),
                    channel_type,
                ),
                String::new(),
                // the C++ passes `true /*st->can_be_edited_*/`; the
                // constructor itself degrades zero-flag rights to a member
                true,
            )
        }
        Some(S::chatMemberStatusMember(_)) => DialogParticipantStatus::member(0, String::new()),
        Some(S::chatMemberStatusRestricted(st)) => {
            let permissions = st.permissions.as_ref();
            DialogParticipantStatus::restricted(
                RestrictedRights::from_bools(
                    permissions.is_some_and(|permissions| permissions.can_send_basic_messages),
                    permissions.is_some_and(|permissions| permissions.can_send_audios),
                    permissions.is_some_and(|permissions| permissions.can_send_documents),
                    permissions.is_some_and(|permissions| permissions.can_send_photos),
                    permissions.is_some_and(|permissions| permissions.can_send_videos),
                    permissions.is_some_and(|permissions| permissions.can_send_video_notes),
                    permissions.is_some_and(|permissions| permissions.can_send_voice_notes),
                    permissions.is_some_and(|permissions| permissions.can_send_other_messages),
                    permissions.is_some_and(|permissions| permissions.can_send_other_messages),
                    permissions.is_some_and(|permissions| permissions.can_send_other_messages),
                    permissions.is_some_and(|permissions| permissions.can_send_other_messages),
                    permissions.is_some_and(|permissions| permissions.can_add_link_previews),
                    permissions.is_some_and(|permissions| permissions.can_send_polls),
                    permissions.is_some_and(|permissions| permissions.can_change_info),
                    permissions.is_some_and(|permissions| permissions.can_invite_users),
                    permissions.is_some_and(|permissions| permissions.can_pin_messages),
                    permissions.is_some_and(|permissions| permissions.can_create_topics),
                    permissions.is_some_and(|permissions| permissions.can_edit_tag),
                    permissions.is_some_and(|permissions| permissions.can_react_to_messages),
                    true,
                    channel_type,
                ),
                st.is_member,
                fix_until_date(st.restricted_until_date),
                channel_type,
                String::new(),
            )
        }
        Some(S::chatMemberStatusLeft(_)) => DialogParticipantStatus::left(),
        Some(S::chatMemberStatusBanned(st)) => {
            DialogParticipantStatus::banned(fix_until_date(st.banned_until_date), String::new())
        }
    }
}

/// `td::DialogParticipants` (DialogParticipant.h) — a member page.
#[derive(Clone, Debug, Default)]
pub struct DialogParticipants {
    pub total_count: i32,
    pub participants: Vec<DialogParticipant>,
}

impl DialogParticipants {
    /// `DialogParticipants::get_chat_members_object` — the per-member object
    /// conversion goes through the chat-manager member plane via the env.
    pub fn get_chat_members_object(
        &self,
        env: &mut impl DialogParticipantManagerEnv,
        source: &str,
    ) -> td_api::ChatMembers {
        env.chat_members_object(self.total_count, self.participants.clone(), source)
    }
}

/// The `Td *td` + `G()` slice of the collapsed DialogParticipantManager.
///
/// The two filter probes come from
/// [`crate::dialog_participant_filter::DialogParticipantFilterEnv`].
pub trait DialogParticipantManagerEnv:
    crate::dialog_participant_filter::DialogParticipantFilterEnv
{
    fn close_flag(&self) -> bool;
    /// `G()->close_status()` — the close gate of the net answer tails.
    fn close_status(&self) -> Status;
    /// `td_->auth_manager_->is_bot()`.
    fn auth_is_bot(&self) -> bool;
    fn now(&self) -> f64;
    fn unix_time(&self) -> i32;
    /// `G()->use_chat_info_database()`.
    fn use_chat_info_database(&self) -> bool;
    fn my_user_id(&self) -> UserId;
    /// `dialog_manager_->get_my_dialog_id()`.
    fn my_dialog_id(&self) -> DialogId;

    // -- DialogManager probes
    /// `dialog_manager_->have_dialog_force(dialog_id, source)`.
    fn have_dialog_force(&mut self, dialog_id: DialogId, source: &str) -> bool;
    /// `dialog_manager_->have_dialog_info(dialog_id)`.
    fn have_dialog_info(&mut self, dialog_id: DialogId) -> bool;
    /// `dialog_manager_->check_dialog_access(dialog_id, false,
    /// AccessRights::Read/Write, source)`.
    fn check_dialog_access_read(&mut self, dialog_id: DialogId, source: &str)
    -> Result<(), Status>;
    fn check_dialog_access_write(
        &mut self,
        dialog_id: DialogId,
        source: &str,
    ) -> Result<(), Status>;
    /// `dialog_manager_->is_monoforum_channel(dialog_id)`.
    fn is_monoforum_channel(&mut self, dialog_id: DialogId) -> bool;
    /// `dialog_manager_->get_dialog_status(dialog_id)`.
    fn get_dialog_status(&mut self, dialog_id: DialogId) -> DialogParticipantStatus;
    /// `messages_manager_->is_dialog_opened(dialog_id)`.
    fn is_dialog_opened(&self, dialog_id: DialogId) -> bool;
    /// `dialog_manager_->get_dialog_search_text(dialog_id)`.
    fn get_dialog_search_text(&self, dialog_id: DialogId) -> String;
    /// `dialog_manager_->get_chat_id_object(dialog_id, source)`.
    fn get_chat_id_object(&mut self, dialog_id: DialogId, source: &str) -> i64;
    /// `dialog_manager_->get_input_peer(dialog_id, AccessRights::Know)`.
    fn get_input_peer_know(&mut self, dialog_id: DialogId) -> Option<telegram_api::InputPeer>;
    /// `dialog_manager_->on_get_dialog_error(dialog_id, status, source)` —
    /// `true` when the error was consumed.
    fn on_get_dialog_error(&mut self, dialog_id: DialogId, status: &Status, source: &str) -> bool;

    // -- ChatManager probes
    fn get_chat_is_active(&mut self, chat_id: ChatId) -> bool;
    fn get_chat_status(&mut self, chat_id: ChatId) -> DialogParticipantStatus;
    fn get_channel_status(&mut self, channel_id: ChannelId) -> DialogParticipantStatus;
    /// `chat_manager_->get_channel_permissions(channel_id)`.
    fn get_channel_permissions(&mut self, channel_id: ChannelId) -> DialogParticipantStatus;
    fn is_broadcast_channel(&mut self, channel_id: ChannelId) -> bool;
    /// `chat_manager_->is_megagroup_channel(channel_id)`.
    fn is_megagroup_channel(&mut self, channel_id: ChannelId) -> bool;
    /// `chat_manager_->get_channel_type(channel_id)`.
    fn get_channel_type(&mut self, channel_id: ChannelId) -> ChannelType;
    /// `chat_manager_->get_channel_effective_has_hidden_participants(
    /// channel_id, source)`.
    fn get_channel_effective_has_hidden_participants(
        &mut self,
        channel_id: ChannelId,
        source: &str,
    ) -> bool;
    /// `chat_manager_->get_input_channel(channel_id)` — the send-leg
    /// resolve; `None` is the C++ nullptr (the 400 "Supergroup not found"
    /// band).
    fn get_input_channel(&mut self, channel_id: ChannelId) -> Option<telegram_api::InputChannel>;
    /// `chat_manager_->invalidate_channel_full(channel_id,
    /// need_drop_slow_mode_delay, source)`.
    fn invalidate_channel_full(
        &mut self,
        channel_id: ChannelId,
        need_drop_slow_mode_delay: bool,
        source: &str,
    );
    /// `chat_manager_->on_get_chats(chats, source)` — the intake.
    fn on_get_chats(&mut self, chats: Vec<telegram_api::Chat>, source: &str);
    /// `chat_manager_->on_get_channel_error(channel_id, status, source)`.
    fn on_get_channel_error(&mut self, channel_id: ChannelId, status: &Status, source: &str);
    /// `chat_manager_->get_chat_member_object(participant, source)`.
    fn get_chat_member_object(
        &mut self,
        participant: &DialogParticipant,
        source: &str,
    ) -> td_api::ChatMember;
    /// `chat_manager_->get_chat_participants(chat_id)` — a snapshot clone,
    /// `None` is the C++ nullptr (the basic-group full info is absent).
    fn get_chat_participants(&mut self, chat_id: ChatId) -> Option<Vec<DialogParticipant>>;
    /// `chat_manager_->load_chat_full(chat_id, false, promise, source)` —
    /// [blocked: net] reload tail; the continuation promise parks offline.
    fn load_chat_full(&mut self, chat_id: ChatId);
    /// `chat_manager_->on_update_channel_bot_user_ids(channel_id, ids)`.
    fn on_update_channel_bot_user_ids(&mut self, channel_id: ChannelId, bot_user_ids: Vec<UserId>);
    /// `chat_manager_->on_update_channel_participant_count(channel_id, count)`.
    fn on_update_channel_participant_count(&mut self, channel_id: ChannelId, count: i32);
    /// `chat_manager_->on_update_channel_administrator_count(channel_id, count)`.
    fn on_update_channel_administrator_count(&mut self, channel_id: ChannelId, count: i32);

    // -- UserManager probes
    /// `user_manager_->have_user_force(user_id, source)`.
    fn have_user_force(&mut self, user_id: UserId, source: &str) -> bool;
    /// `user_manager_->is_user_deleted(user_id)`.
    fn is_user_deleted(&mut self, user_id: UserId) -> bool;
    /// `user_manager_->have_min_user(user_id)`.
    fn have_min_user(&mut self, user_id: UserId) -> bool;
    /// `user_manager_->get_user_was_online(user_id, unix_time)`.
    fn get_user_was_online(&mut self, user_id: UserId, unix_time: i32) -> i32;
    /// `user_manager_->is_user_online(user_id, 0, unix_time)` — the
    /// online-member-count walker probe.
    fn is_user_online(&mut self, user_id: UserId, unix_time: i32) -> bool;
    /// `user_manager_->get_input_user(user_id)` — the EditChatCreator send
    /// leg.
    fn get_input_user(&mut self, user_id: UserId) -> Result<telegram_api::InputUser, Status>;
    /// `user_manager_->on_get_users(users, source)` — the intake.
    fn on_get_users(&mut self, users: Vec<telegram_api::User>, source: &str);
    /// `user_manager_->get_secret_chat_user_id(secret_chat_id)`.
    fn get_secret_chat_user_id(&mut self, dialog_id: DialogId) -> UserId;
    /// `DialogAdministrator::get_chat_administrator_object(user_manager)`.
    fn chat_administrator_object(
        &mut self,
        administrator: &DialogAdministrator,
    ) -> td_api::ChatAdministrator;
    /// `status.update_restrictions(G())` — the env carries the Global.
    fn status_update_restrictions(&mut self, status: &mut DialogParticipantStatus);

    // -- messages_manager
    /// `messages_manager_->have_dialog(dialog_id)`.
    fn have_dialog(&mut self, dialog_id: DialogId) -> bool;

    // -- story_manager intake
    /// `story_manager_->on_view_dialog_active_stories(dialog_ids)`.
    fn on_view_dialog_active_stories(&mut self, dialog_ids: Vec<DialogId>);

    // -- the object-conversion tail (the chat_manager/user_manager plane)
    /// `DialogParticipants::get_chat_members_object`'s per-page conversion.
    fn chat_members_object(
        &mut self,
        total_count: i32,
        participants: Vec<DialogParticipant>,
        source: &str,
    ) -> td_api::ChatMembers;
    /// the `promise.set_value(nullptr user)` leg of
    /// `get_future_creator_after_leave`.
    fn send_null_user(&mut self, promise: Promise<td_api::User>);

    // -- the sqlite pmc
    fn sqlite_pmc_get_raw(&mut self, key: &[u8]) -> Vec<u8>;
    fn sqlite_pmc_set_raw(&mut self, key: &[u8], value: &[u8]);
    fn sqlite_pmc_erase(&mut self, key: &[u8]);

    // -- the two MultiTimeouts; the Rust tick pattern feeds them deadlines
    /// `update_dialog_online_member_count_timeout_.set(timeout, dialog_id)`;
    /// a zero timeout cancels.
    fn set_online_member_count_timeout(&mut self, dialog_id: DialogId, timeout: f64);
    /// `channel_participant_cache_timeout_.set(timeout, channel_id)`.
    fn set_channel_participant_cache_timeout(&mut self, channel_id: ChannelId, timeout: f64);

    // -- the update out machine
    /// `send_update_chat_online_member_count` (the updateChatOnlineMemberCount
    /// out machine; the C++ guard is inlined at the call site).
    fn send_update_chat_online_member_count(
        &mut self,
        dialog_id: DialogId,
        online_member_count: i32,
    );

    // -- the wire query seams. The three net-drain 35 families dispatch
    // -- through the net handle (the `Err` return is the send-leg resolve
    // -- failure: the manager settles the just-parked lot head with it, the
    // -- C++ handler-creation order keeps the promise ahead of `->send`);
    // -- the handle-less env keeps the net-inert posture. The rest stay
    // -- [blocked: net] — each parks its promise, the C++ request without a
    // -- network behaves identically.
    /// `GetChannelAdministratorsQuery::send(channel_id, hash)`; the result
    /// re-enters `on_reload_dialog_administrators`.
    fn get_channel_administrators_query(&mut self, channel_id: ChannelId, hash: i64);
    /// `GetChannelParticipantsQuery::send` (cpp:407-416) — the input
    /// channel resolves here (`None` → the 400 "Supergroup not found"
    /// band); the answer re-enters `on_get_channel_participants_answer`.
    fn send_get_channel_participants_query(
        &mut self,
        channel_id: ChannelId,
        filter: &ChannelParticipantFilter,
        offset: i32,
        limit: i32,
    ) -> Result<(), Status> {
        let _ = (channel_id, filter, offset, limit);
        tdutils::log_info!("Send channels.getParticipants — [net-inert]");
        Ok(())
    }
    /// `GetChannelParticipantQuery::send` (cpp:351-363) — the input peer
    /// arrives pre-resolved; the input channel resolves here (`None` → the
    /// 400 "Supergroup not found" band); the answer re-enters
    /// `on_get_channel_participant_answer`.
    fn send_get_channel_participant_query(
        &mut self,
        channel_id: ChannelId,
        input_peer: telegram_api::InputPeer,
    ) -> Result<(), Status> {
        let _ = (channel_id, input_peer);
        tdutils::log_info!("Send channels.getParticipant — [net-inert]");
        Ok(())
    }
    /// `EditChatCreatorQuery::send` (cpp:913-926) — the peer/user resolve
    /// here; the peer band reenters the C++ `on_error` tail through the
    /// completed queue (the dialog-keyed intake), the user band settles the
    /// parked promise directly. The answer re-enters
    /// `on_edit_chat_creator_answer`.
    fn send_edit_chat_creator_query(
        &mut self,
        dialog_id: DialogId,
        user_id: UserId,
        input_check_password: telegram_api::InputCheckPasswordSRP,
    ) -> Result<(), Status> {
        let _ = (dialog_id, user_id, input_check_password);
        tdutils::log_info!("Send messages.editChatCreator — [net-inert]");
        Ok(())
    }
    /// `GetChatJoinRequestsQuery::send(...)`.
    fn get_chat_join_requests_query(
        &mut self,
        dialog_id: DialogId,
        invite_link: String,
        query: String,
        offset_date: i32,
        offset_user_id: UserId,
        limit: i32,
    );
    /// `HideChatJoinRequestQuery::send(dialog_id, user_id, approve)`.
    fn hide_chat_join_request_query(&mut self, dialog_id: DialogId, user_id: UserId, approve: bool);
    /// `HideAllChatJoinRequestsQuery::send(dialog_id, invite_link, approve)`.
    fn hide_all_chat_join_requests_query(
        &mut self,
        dialog_id: DialogId,
        invite_link: String,
        approve: bool,
    );
    /// `CanEditChatCreatorQuery::send()` — the STATUS error comes back into
    /// [`DialogParticipantManager::map_can_transfer_ownership_error`];
    /// offline it parks.
    fn can_edit_chat_creator_query(&mut self);
    /// `GetFutureChatCreatorAfterLeaveQuery::send(dialog_id)`.
    fn get_future_chat_creator_query(&mut self, dialog_id: DialogId);
    /// `PasswordManager::get_input_check_password_srp(password, ...)` — the
    /// SRP half of the ownership transfer stays [blocked: PasswordManager];
    /// the promise parks in the manager's lot until the SRP resolves (the
    /// C++ request awaiting its own net answer), and the resolved-SRP tail
    /// is [`DialogParticipantManager::transfer_chat_ownership`].
    fn transfer_chat_ownership_seam(
        &mut self,
        dialog_id: DialogId,
        user_id: UserId,
        password: String,
    );
    /// `EditChatParticipantRankQuery::send(dialog_id, user_id, rank)`.
    fn edit_chat_participant_rank_query(
        &mut self,
        dialog_id: DialogId,
        user_id: UserId,
        rank: String,
    );
    /// `RequestChatJoinWebViewQuery::send(query_id, parameters)`.
    fn request_chat_join_web_view_query(&mut self, query_id: i64);
    /// `SetJoinChatResultsQuery::send(query_id, result, url)`.
    fn set_join_chat_results_query(
        &mut self,
        query_id: i64,
        result: td_api::ChatJoinRequestResult,
        url: String,
    );
    /// the basic-group join/add wire send (`messages.addChatUser` and the
    /// invite-link acceptance pre-query) — the C++ decision machinery above
    /// the send stays local in the manager.
    fn join_or_add_chat_member_query(&mut self, chat_id: ChatId, user_id: UserId);
    /// the channel join wire send (`channels.joinChannel`), with the
    /// pending-query vector consumed by the response [blocked: net].
    fn join_channel_query(&mut self, channel_id: ChannelId);
    /// the channel add wire send (`channels.inviteToChannel`), result
    /// re-enters `speculative_add_channel_user` [blocked: net].
    fn invite_to_channel_query(&mut self, channel_id: ChannelId, user_id: UserId);
    /// the channel status-edit wire send (`channels.editCreator` /
    /// `editAdmin` / `editBanned` per the C++
    /// set_channel_participant_status_impl).
    fn edit_channel_participant_query(
        &mut self,
        channel_id: ChannelId,
        participant_dialog_id: DialogId,
        new_status: DialogParticipantStatus,
    );
    /// the basic-group status-edit wire send (`messages.deleteChatUser` /
    /// `messages.editChatAdmin` / the re-invite per
    /// `set_chat_participant_status`).
    fn edit_chat_participant_query(
        &mut self,
        chat_id: ChatId,
        user_id: UserId,
        new_status: DialogParticipantStatus,
    );
}

/// `DialogParticipantManager::OnlineMemberCountInfo`
/// (DialogParticipantManager.h:309-313).
#[derive(Clone, Copy, Debug, Default)]
pub struct OnlineMemberCountInfo {
    pub online_member_count: i32,
    pub update_time: f64,
    pub is_update_sent: bool,
}

/// The DialogParticipant-plane in-flight query kinds — the `Td::ResultHandler`
/// classes of the net-drain 35 families (DialogParticipantManager.cpp:342-443,
/// 904-946). The completions park by kind behind the Td-held net handle
/// (`crate::td::DialogParticipantNetHandle`); `Td::on_dialog_participant_net_answer`
/// routes them. The per-query handler members (the promises and the
/// `channel_id_`/`participant_dialog_id_`/`dialog_id_`/`user_id_` state the
/// C++ handlers read on the answer legs) park in the manager's FIFO lots
/// ([`PendingDialogParticipantQueries`]) — the kinds stay `Copy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DialogParticipantNetKind {
    /// `GetChannelParticipantQuery` (cpp:342-395) — `channels.getParticipant`.
    ChannelParticipant,
    /// `GetChannelParticipantsQuery` (cpp:397-443) — `channels.getParticipants`.
    ChannelParticipants,
    /// `EditChatCreatorQuery` (cpp:904-946) — `messages.editChatCreator`.
    EditChatCreator,
}

/// The parked query promises — the ResultHandler members the `Copy`
/// [`DialogParticipantNetKind`]s can't carry. FIFO-paired per family (§5).
#[derive(Default)]
pub(crate) struct PendingDialogParticipantQueries {
    /// `GetChannelParticipantQuery`'s `channel_id_`/`participant_dialog_id_`
    /// plus the promise chain tail (the collapsed chatMember promise).
    pub(crate) channel_participant: Vec<(ChannelId, DialogId, Promise<td_api::ChatMember>)>,
    /// `GetChannelParticipantsQuery`'s `channel_id_` and the lambda-carried
    /// filter/offset/limit/additional pair (cpp:2053-2064) plus the
    /// conversion source tag of the caller's collapsing lambda.
    pub(crate) channel_participants: Vec<PendingChannelParticipantsQuery>,
    /// `EditChatCreatorQuery`'s `dialog_id_`/`user_id_`/promise — only
    /// wire-sent queries park here (the resolved-SRP tail of
    /// [`DialogParticipantManager::transfer_chat_ownership`]), so the FIFO
    /// pairing stays exact.
    pub(crate) edit_chat_creator: Vec<(DialogId, UserId, Promise<()>)>,
    /// The promises parked at the [blocked: PasswordManager] SRP half of
    /// `transfer_dialog_ownership` — a separate lot from the wire-sent
    /// `edit_chat_creator` queries (the C++ handler only exists once the
    /// SRP resolves; mixing the parks would mis-pair the FIFO).
    pub(crate) transfer_ownership_srp: Vec<(DialogId, UserId, Promise<()>)>,
}

/// A parked `channels.getParticipants` page request —
/// `get_channel_participants`'s lambda captures (cpp:2053-2064) plus the
/// conversion source the caller's collapsing lambda used
/// ("getSupergroupMembers" / "searchChatMembers").
pub(crate) struct PendingChannelParticipantsQuery {
    pub(crate) channel_id: ChannelId,
    pub(crate) filter: ChannelParticipantFilter,
    pub(crate) offset: i32,
    pub(crate) limit: i32,
    pub(crate) additional_query: String,
    pub(crate) additional_limit: i32,
    pub(crate) source: &'static str,
    pub(crate) promise: Promise<td_api::ChatMembers>,
}

/// `td::DialogParticipantManager` (DialogParticipantManager.h:37).
#[derive(Default)]
pub struct DialogParticipantManager {
    /// `dialog_online_member_counts_`.
    pub(crate) dialog_online_member_counts: HashMap<i64, OnlineMemberCountInfo>,
    /// `dialog_administrators_`.
    pub(crate) dialog_administrators: HashMap<i64, Vec<DialogAdministrator>>,
    /// `channel_participants_` — the bot-administrators-only cache:
    /// channel id -> (participant dialog id -> (participant, last_access_date)).
    pub(crate) channel_participants: HashMap<i64, HashMap<i64, (DialogParticipant, i32)>>,
    /// `last_channel_participant_update_time_` — written by the
    /// [blocked: net] response intake; nothing reads it offline yet.
    #[allow(dead_code)]
    pub(crate) last_channel_participant_update_time: f64,
    /// the deadlines fed to `update_dialog_online_member_count_timeout_`
    /// (dialog id -> absolute deadline), driven by the Td tick.
    pub(crate) online_member_count_deadlines: HashMap<i64, f64>,
    /// the deadlines fed to `channel_participant_cache_timeout_`.
    pub(crate) channel_participant_cache_deadlines: HashMap<i64, f64>,
    /// `cached_channel_participants_` — the megagroup recent-page cache the
    /// online-member-count refresh reads
    /// (DialogParticipantManager.h:334).
    pub(crate) cached_channel_participants: HashMap<i64, Vec<DialogParticipant>>,
    /// `user_online_member_dialogs_` — the user -> (dialog id -> seen-at)
    /// map the per-user refire timeout consumes (cpp:1226-1245); the
    /// timeout consumer stays [blocked: the user-timeout plane], the
    /// server-side writes land with the net-drain 35 intake.
    #[allow(dead_code)]
    pub(crate) user_online_member_dialogs: HashMap<i64, HashMap<i64, i32>>,
    /// the parked query promises of the net-drain 35 families.
    pub(crate) pending: PendingDialogParticipantQueries,
}

impl DialogParticipantManager {
    pub fn new() -> DialogParticipantManager {
        DialogParticipantManager::default()
    }

    // -- the online member count plane -----------------------------------

    /// `DialogParticipantManager::on_update_dialog_online_member_count`
    /// (DialogParticipantManager.cpp:1055-1080) — the M.7 fan-out intake.
    pub fn on_update_dialog_online_member_count(
        &mut self,
        env: &mut impl DialogParticipantManagerEnv,
        dialog_id: DialogId,
        online_member_count: i32,
        is_from_server: bool,
    ) {
        let info = self
            .dialog_online_member_counts
            .entry(dialog_id.get())
            .or_default();
        if info.online_member_count == online_member_count {
            return;
        }
        info.online_member_count = online_member_count;
        if is_from_server {
            info.update_time = env.now();
        } else if info.is_update_sent {
            let update_time = info.update_time;
            let is_update_sent = info.is_update_sent;
            let _ = (update_time, is_update_sent);
            self.set_dialog_online_member_count(
                env,
                dialog_id,
                online_member_count,
                is_from_server,
            );
        }
    }

    /// `DialogParticipantManager::set_dialog_online_member_count`
    /// (DialogParticipantManager.cpp:1103-1140).
    fn set_dialog_online_member_count(
        &mut self,
        env: &mut impl DialogParticipantManagerEnv,
        dialog_id: DialogId,
        online_member_count: i32,
        is_from_server: bool,
    ) {
        let now = env.now();
        let info = self
            .dialog_online_member_counts
            .entry(dialog_id.get())
            .or_default();
        let need_update = !info.is_update_sent
            || now > info.update_time + ONLINE_MEMBER_COUNT_CACHE_EXPIRE_TIME as f64;
        if need_update {
            info.update_time = now;
            info.is_update_sent = false;
            let timeout = ONLINE_MEMBER_COUNT_UPDATE_TIME as f64;
            self.online_member_count_deadlines
                .insert(dialog_id.get(), now + timeout);
            env.set_online_member_count_timeout(dialog_id, timeout);
        }
        if need_update || is_from_server {
            env.send_update_chat_online_member_count(dialog_id, online_member_count);
            info.is_update_sent = true;
        }
    }

    /// the `update_dialog_online_member_count_timeout_` callback — the Td
    /// tick entry.
    pub fn on_online_member_count_timeout(
        &mut self,
        env: &mut impl DialogParticipantManagerEnv,
        dialog_id: DialogId,
    ) {
        self.update_dialog_online_member_count(env, dialog_id, false);
    }

    /// `DialogParticipantManager::update_dialog_online_member_count`
    /// (DialogParticipantManager.cpp:1150-1166) — the cache-expiry refresh.
    pub fn update_dialog_online_member_count(
        &mut self,
        env: &mut impl DialogParticipantManagerEnv,
        dialog_id: DialogId,
        is_from_server: bool,
    ) {
        let Some(info) = self
            .dialog_online_member_counts
            .get(&dialog_id.get())
            .copied()
        else {
            return;
        };
        if env.now() > info.update_time + ONLINE_MEMBER_COUNT_CACHE_EXPIRE_TIME as f64 {
            self.online_member_count_deadlines.remove(&dialog_id.get());
            env.set_online_member_count_timeout(dialog_id, 0.0);
            self.dialog_online_member_counts.remove(&dialog_id.get());
        } else if info.is_update_sent {
            self.set_dialog_online_member_count(
                env,
                dialog_id,
                info.online_member_count,
                is_from_server,
            );
        }
    }

    // -- the administrators plane ----------------------------------------

    /// `DialogParticipantManager::get_dialog_administrators_database_key`
    /// (DialogParticipantManager.cpp:1481-1483) — `PSTRING() << "adm" <<
    /// -dialog_id.get()`.
    pub fn get_dialog_administrators_database_key(dialog_id: DialogId) -> Vec<u8> {
        format!("adm{}", -(dialog_id.get())).into_bytes()
    }

    /// `DialogParticipantManager::get_chat_administrators_object`
    /// (DialogParticipantManager.cpp:1429-1435).
    fn get_chat_administrators_object(
        &self,
        env: &mut impl DialogParticipantManagerEnv,
        administrators: &[DialogAdministrator],
    ) -> td_api::ChatAdministrators {
        td_api::ChatAdministrators {
            administrators: administrators
                .iter()
                .map(|administrator| env.chat_administrator_object(administrator))
                .collect(),
        }
    }

    /// `DialogParticipantManager::get_dialog_administrators`
    /// (DialogParticipantManager.cpp:1437-1479).
    pub fn get_dialog_administrators(
        &mut self,
        env: &mut impl DialogParticipantManagerEnv,
        dialog_id: DialogId,
        promise: Promise<td_api::ChatAdministrators>,
    ) {
        let mut promise = Some(promise);
        if !env.have_dialog_force(dialog_id, "get_dialog_administrators") {
            return promise
                .take()
                .unwrap()
                .set_error(Status::error(400, "Chat not found"));
        }

        match dialog_id.get_type() {
            DialogType::User | DialogType::SecretChat => {
                return promise
                    .take()
                    .unwrap()
                    .set_value(td_api::ChatAdministrators {
                        administrators: Vec::new(),
                    });
            }
            DialogType::Chat => {}
            DialogType::Channel => {
                if env.is_monoforum_channel(dialog_id) {
                    return promise
                        .take()
                        .unwrap()
                        .set_value(td_api::ChatAdministrators {
                            administrators: Vec::new(),
                        });
                }
            }
            DialogType::None => unreachable!(),
        }

        if let Some(administrators) = self.dialog_administrators.get(&dialog_id.get()).cloned() {
            // `reload_dialog_administrators(dialog_id, it->second, Auto())` —
            // the fire-and-forget cache refresh
            self.reload_dialog_administrators(env, dialog_id, &administrators, None);
            let object = self.get_chat_administrators_object(env, &administrators);
            return promise.take().unwrap().set_value(object);
        }

        if env.use_chat_info_database() {
            let value =
                env.sqlite_pmc_get_raw(&Self::get_dialog_administrators_database_key(dialog_id));
            return self.on_load_dialog_administrators_from_database(
                env,
                dialog_id,
                value,
                promise.take().unwrap(),
            );
        }

        self.reload_dialog_administrators(env, dialog_id, &[], promise.take());
    }

    /// `DialogParticipantManager::on_load_dialog_administrators_from_database`
    /// (DialogParticipantManager.cpp:1485-1516). The user-load multipromise
    /// collapses to the env user probes: an unloaded administrator user is
    /// the `Result<Unit>` error leg that routes into the reload.
    fn on_load_dialog_administrators_from_database(
        &mut self,
        env: &mut impl DialogParticipantManagerEnv,
        dialog_id: DialogId,
        value: Vec<u8>,
        promise: Promise<td_api::ChatAdministrators>,
    ) {
        if value.is_empty() {
            return self.reload_dialog_administrators(env, dialog_id, &[], Some(promise));
        }
        let mut parser = Parser::new(&value);
        let administrators = match parse_dialog_administrators(&mut parser) {
            Ok(administrators) => administrators,
            Err(_) => return self.reload_dialog_administrators(env, dialog_id, &[], Some(promise)),
        };

        // the `LoadUsersMultiPromiseActor` join: every administrator user
        // must load (the C++ `get_user(user_id, 3, ...)`); any failure is
        // the reload leg
        if !administrators
            .iter()
            .all(|administrator| env.have_min_user(administrator.get_user_id()))
        {
            return self.reload_dialog_administrators(env, dialog_id, &[], Some(promise));
        }
        self.on_load_administrator_users_finished(env, dialog_id, administrators, promise);
    }

    /// `DialogParticipantManager::on_load_administrator_users_finished`
    /// (DialogParticipantManager.cpp:1518-1530).
    fn on_load_administrator_users_finished(
        &mut self,
        env: &mut impl DialogParticipantManagerEnv,
        dialog_id: DialogId,
        administrators: Vec<DialogAdministrator>,
        promise: Promise<td_api::ChatAdministrators>,
    ) {
        let entry = self
            .dialog_administrators
            .entry(dialog_id.get())
            .or_insert_with(|| administrators.clone())
            .clone();
        self.reload_dialog_administrators(env, dialog_id, &entry, None);
        let object = self.get_chat_administrators_object(env, &entry);
        let mut promise = promise;
        promise.set_value(object);
    }

    /// `DialogParticipantManager::on_update_dialog_administrators`
    /// (DialogParticipantManager.cpp:1532-1564).
    pub fn on_update_dialog_administrators(
        &mut self,
        env: &mut impl DialogParticipantManagerEnv,
        dialog_id: DialogId,
        mut administrators: Vec<DialogAdministrator>,
        have_access: bool,
        from_database: bool,
    ) {
        if have_access {
            debug_assert!(dialog_id.is_valid());
            administrators.sort_by_key(|administrator| administrator.get_user_id().get());

            let unchanged = self
                .dialog_administrators
                .get(&dialog_id.get())
                .is_some_and(|stored| *stored == administrators);
            if unchanged {
                return;
            }
            if env.use_chat_info_database() && !from_database {
                let mut out = Vec::new();
                store_dialog_administrators(&administrators, &mut out);
                env.sqlite_pmc_set_raw(
                    &Self::get_dialog_administrators_database_key(dialog_id),
                    &out,
                );
            }
            self.dialog_administrators
                .insert(dialog_id.get(), administrators);
        } else {
            self.dialog_administrators.remove(&dialog_id.get());
            if env.use_chat_info_database() {
                env.sqlite_pmc_erase(&Self::get_dialog_administrators_database_key(dialog_id));
            }
        }
    }

    /// `DialogParticipantManager::reload_dialog_administrators`
    /// (DialogParticipantManager.cpp:1566-1605). `promise` is `None` for the
    /// C++ `Auto()` fire-and-forget refresh; the reload tails are the
    /// [blocked: net] seams.
    pub fn reload_dialog_administrators(
        &mut self,
        env: &mut impl DialogParticipantManagerEnv,
        dialog_id: DialogId,
        dialog_administrators: &[DialogAdministrator],
        mut promise: Option<Promise<td_api::ChatAdministrators>>,
    ) {
        let dialog_type = dialog_id.get_type();
        if env.is_monoforum_channel(dialog_id)
            || (dialog_type == DialogType::Chat
                && !env.get_chat_status(dialog_id.get_chat_id()).is_member())
        {
            if let Some(mut promise) = promise.take() {
                promise.set_value(td_api::ChatAdministrators {
                    administrators: Vec::new(),
                });
            }
            return;
        }
        match dialog_type {
            DialogType::Chat => {
                // `load_chat_full(...)` then `on_reload_dialog_administrators`;
                // the reload tail parks offline
                env.load_chat_full(dialog_id.get_chat_id());
            }
            DialogType::Channel => {
                let channel_id = dialog_id.get_channel_id();
                if env.is_broadcast_channel(channel_id)
                    && !env.get_channel_status(channel_id).is_administrator()
                {
                    if let Some(mut promise) = promise.take() {
                        promise.set_error(Status::error(400, "Administrator list is inaccessible"));
                    }
                    return;
                }
                // `get_vector_hash(user ids)` — the C++ query argument
                let hash = get_vector_hash(
                    &dialog_administrators
                        .iter()
                        .map(|administrator| administrator.get_user_id().get() as u64)
                        .collect::<Vec<u64>>(),
                );
                env.get_channel_administrators_query(channel_id, hash);
            }
            _ => unreachable!(),
        }
    }

    /// `DialogParticipantManager::speculative_update_dialog_administrators`
    /// (DialogParticipantManager.cpp:1372-1409).
    pub fn speculative_update_dialog_administrators(
        &mut self,
        env: &mut impl DialogParticipantManagerEnv,
        dialog_id: DialogId,
        user_id: UserId,
        new_status: &DialogParticipantStatus,
        old_status: &DialogParticipantStatus,
    ) {
        if new_status.is_administrator_member() == old_status.is_administrator_member() {
            return;
        }
        let Some(administrators) = self.dialog_administrators.get(&dialog_id.get()) else {
            return;
        };
        let mut administrators = administrators.clone();
        if new_status.is_administrator_member() {
            let mut is_found = false;
            for administrator in &mut administrators {
                if administrator.get_user_id() == user_id {
                    is_found = true;
                    if administrator.is_creator() != new_status.is_creator() {
                        *administrator = DialogAdministrator::new(
                            user_id,
                            administrator.get_rank().to_owned(),
                            new_status.is_creator(),
                            true,
                        );
                        self.on_update_dialog_administrators(
                            env,
                            dialog_id,
                            administrators.clone(),
                            true,
                            false,
                        );
                    }
                    break;
                }
            }
            if !is_found {
                administrators.push(DialogAdministrator::new(
                    user_id,
                    String::new(),
                    new_status.is_creator(),
                    true,
                ));
                self.on_update_dialog_administrators(env, dialog_id, administrators, true, false);
            }
        } else if let Some(index) = administrators
            .iter()
            .position(|administrator| administrator.get_user_id() == user_id)
        {
            administrators.remove(index);
            self.on_update_dialog_administrators(env, dialog_id, administrators, true, false);
        }
    }

    /// `DialogParticipantManager::speculative_update_dialog_administrator_rank`
    /// (DialogParticipantManager.cpp:1411-1427).
    pub fn speculative_update_dialog_administrator_rank(
        &mut self,
        env: &mut impl DialogParticipantManagerEnv,
        dialog_id: DialogId,
        user_id: UserId,
        new_rank: &str,
    ) {
        let Some(administrators) = self.dialog_administrators.get(&dialog_id.get()) else {
            return;
        };
        let mut administrators = administrators.clone();
        for administrator in &mut administrators {
            if administrator.get_user_id() == user_id {
                if administrator.get_rank() != new_rank {
                    administrator.set_rank(new_rank);
                    self.on_update_dialog_administrators(
                        env,
                        dialog_id,
                        administrators,
                        true,
                        false,
                    );
                }
                break;
            }
        }
    }

    // -- the member query plane -------------------------------------------

    /// `DialogParticipantManager::get_dialog_participant` +
    /// `finish_get_dialog_participant` (DialogParticipantManager.cpp:1804-1827).
    pub fn get_dialog_participant(
        &mut self,
        env: &mut impl DialogParticipantManagerEnv,
        dialog_id: DialogId,
        participant_dialog_id: DialogId,
        mut promise: Promise<td_api::ChatMember>,
    ) {
        if !env.have_dialog_force(dialog_id, "do_get_dialog_participant") {
            return promise.set_error(Status::error(400, "Chat not found"));
        }

        match dialog_id.get_type() {
            DialogType::User => {
                let my_user_id = env.my_user_id();
                let peer_user_id = dialog_id.get_user_id();
                let participant = if participant_dialog_id == DialogId::from_user_id(my_user_id) {
                    DialogParticipant::private_member(my_user_id, peer_user_id)
                } else if participant_dialog_id == dialog_id {
                    DialogParticipant::private_member(peer_user_id, my_user_id)
                } else {
                    return promise.set_error(Status::error(400, "Member not found"));
                };
                self.finish_get_dialog_participant(env, participant, promise);
            }
            DialogType::Chat => {
                if participant_dialog_id.get_type() != DialogType::User {
                    let participant = DialogParticipant::left(participant_dialog_id);
                    return self.finish_get_dialog_participant(env, participant, promise);
                }
                // `chat_manager_->get_chat_participant(chat_id, user_id,
                // promise)` — the in-memory answer finishes; otherwise the
                // db-load/reload tail parks the promise [blocked: net]
                match env.get_chat_participants(dialog_id.get_chat_id()) {
                    Some(participants) => {
                        if let Some(participant) = participants
                            .iter()
                            .find(|participant| participant.dialog_id_ == participant_dialog_id)
                        {
                            let participant = participant.clone();
                            return self.finish_get_dialog_participant(env, participant, promise);
                        }
                        promise.set_error(Status::error(400, "Member not found"));
                    }
                    None => {
                        env.load_chat_full(dialog_id.get_chat_id());
                    }
                }
            }
            DialogType::Channel => {
                self.get_channel_participant(
                    env,
                    dialog_id.get_channel_id(),
                    participant_dialog_id,
                    promise,
                );
            }
            DialogType::SecretChat => {
                let my_user_id = env.my_user_id();
                let peer_user_id = env.get_secret_chat_user_id(dialog_id);
                let participant = if participant_dialog_id == DialogId::from_user_id(my_user_id) {
                    DialogParticipant::private_member(my_user_id, peer_user_id)
                } else if peer_user_id.is_valid()
                    && participant_dialog_id == DialogId::from_user_id(peer_user_id)
                {
                    DialogParticipant::private_member(peer_user_id, my_user_id)
                } else {
                    return promise.set_error(Status::error(400, "Member not found"));
                };
                self.finish_get_dialog_participant(env, participant, promise);
            }
            DialogType::None => unreachable!(),
        }
    }

    /// `DialogParticipantManager::finish_get_dialog_participant`
    /// (DialogParticipantManager.cpp:1815-1827).
    fn finish_get_dialog_participant(
        &mut self,
        env: &mut impl DialogParticipantManagerEnv,
        dialog_participant: DialogParticipant,
        mut promise: Promise<td_api::ChatMember>,
    ) {
        // `TRY_STATUS_PROMISE(promise, G()->close_status())`
        // (DialogParticipantManager.cpp:1817-1819)
        let close_status = env.close_status();
        if close_status.is_error() {
            return promise.set_error(close_status);
        }
        let participant_dialog_id = dialog_participant.dialog_id_;
        let is_user = participant_dialog_id.get_type() == DialogType::User;
        if (is_user && !env.have_min_user(participant_dialog_id.get_user_id()))
            || (!is_user && !env.have_dialog(participant_dialog_id))
        {
            return promise.set_error(Status::error(400, "Member not found"));
        }
        let object =
            env.get_chat_member_object(&dialog_participant, "finish_get_dialog_participant");
        promise.set_value(object);
    }

    /// `DialogParticipantManager::get_channel_participant`
    /// (DialogParticipantManager.cpp:1876-1915). The bot fast path is the
    /// C++ `if (false && ...)` dead branch — kept dead the same way.
    pub fn get_channel_participant(
        &mut self,
        env: &mut impl DialogParticipantManagerEnv,
        channel_id: ChannelId,
        participant_dialog_id: DialogId,
        mut promise: Promise<td_api::ChatMember>,
    ) {
        tdutils::log_info!(
            "Trying to get {:?} as member of {:?}",
            participant_dialog_id,
            channel_id
        );
        let Some(input_peer) = env.get_input_peer_know(participant_dialog_id) else {
            return promise.set_error(Status::error(400, "Member not found"));
        };
        if env.is_broadcast_channel(channel_id)
            && !env.get_channel_status(channel_id).is_administrator()
        {
            return promise.set_error(Status::error(400, "Member list is inaccessible"));
        }

        if self.have_channel_participant_cache(channel_id)
            && let Some(participant) =
                self.get_channel_participant_from_cache(channel_id, participant_dialog_id)
        {
            // the C++ `promise.set_value(DialogParticipant{*participant})`
            // settles the caller-side lambda — only the dialog-level
            // finish runs (the channel-level tail belongs to the query
            // answer alone, cpp:1889-1894)
            let participant = participant.clone();
            return self.finish_get_dialog_participant(env, participant, promise);
        }

        // the C++ handler carries the promise before `->send` resolves the
        // input channel — park first, then settle the lot head on the
        // send-leg failure band (the promise tail is the collapsed
        // finish_get_channel_participant chain, cpp:1905-1911)
        self.pending
            .channel_participant
            .push((channel_id, participant_dialog_id, promise));
        if let Err(error) = env.send_get_channel_participant_query(channel_id, input_peer) {
            let (_, _, mut promise) = self.pending.channel_participant.remove(0);
            promise.set_error(error);
        }
    }

    // -- the member search plane ------------------------------------------

    /// `DialogParticipantManager::search_among_dialogs`
    /// (DialogParticipantManager.cpp:1938-1960).
    fn search_among_dialogs(
        &self,
        env: &mut impl DialogParticipantManagerEnv,
        dialog_ids: &[DialogId],
        query: &str,
        limit: i32,
    ) -> (i32, Vec<DialogId>) {
        let mut hints = Hints::default();
        let unix_time = env.unix_time();
        for &dialog_id in dialog_ids {
            if !env.have_dialog_info(dialog_id) {
                continue;
            }
            if query.is_empty() {
                hints.add(dialog_id.get(), " ");
            } else {
                hints.add(dialog_id.get(), &env.get_dialog_search_text(dialog_id));
            }
            if dialog_id.get_type() == DialogType::User {
                hints.set_rating(
                    dialog_id.get(),
                    -(env.get_user_was_online(dialog_id.get_user_id(), unix_time) as i64),
                );
            }
        }

        let (total_count, keys) = hints.search(query, limit, true);
        (
            total_count as i32,
            keys.into_iter().map(DialogId::new).collect(),
        )
    }

    /// `DialogParticipantManager::search_private_chat_participants`
    /// (DialogParticipantManager.cpp:1962-1980).
    fn search_private_chat_participants(
        &self,
        env: &mut impl DialogParticipantManagerEnv,
        peer_user_id: UserId,
        query: &str,
        limit: i32,
        filter: &DialogParticipantFilter,
    ) -> DialogParticipants {
        let my_user_id = env.my_user_id();
        let mut dialog_ids = Vec::new();
        if filter.is_dialog_participant_suitable(
            env,
            &DialogParticipant::private_member(my_user_id, peer_user_id),
        ) {
            dialog_ids.push(DialogId::from_user_id(my_user_id));
        }
        if peer_user_id.is_valid()
            && peer_user_id != my_user_id
            && filter.is_dialog_participant_suitable(
                env,
                &DialogParticipant::private_member(peer_user_id, my_user_id),
            )
        {
            dialog_ids.push(DialogId::from_user_id(peer_user_id));
        }

        let (total_count, dialog_ids) = self.search_among_dialogs(env, &dialog_ids, query, limit);
        let participants = dialog_ids
            .into_iter()
            .map(|dialog_id| {
                let user_id = dialog_id.get_user_id();
                DialogParticipant::private_member(
                    user_id,
                    if user_id == my_user_id {
                        peer_user_id
                    } else {
                        my_user_id
                    },
                )
            })
            .collect();
        DialogParticipants {
            total_count,
            participants,
        }
    }

    /// `DialogParticipantManager::search_chat_participants` +
    /// `do_search_chat_participants` (DialogParticipantManager.cpp:1982-2031).
    fn search_chat_participants(
        &mut self,
        env: &mut impl DialogParticipantManagerEnv,
        chat_id: ChatId,
        query: &str,
        limit: i32,
        filter: &DialogParticipantFilter,
        mut promise: Promise<td_api::ChatMembers>,
    ) {
        if limit < 0 {
            return promise.set_error(Status::error(400, "Parameter limit must be non-negative"));
        }

        // the C++ awaits `load_chat_full` before searching; only the
        // in-memory full info answers offline, the reload tail parks
        let Some(participants) = env.get_chat_participants(chat_id) else {
            env.load_chat_full(chat_id);
            return;
        };

        let mut dialog_ids = Vec::new();
        for participant in &participants {
            if filter.is_dialog_participant_suitable(env, participant) {
                dialog_ids.push(participant.dialog_id_);
            }
        }

        let (total_count, dialog_ids) = self.search_among_dialogs(env, &dialog_ids, query, limit);
        env.on_view_dialog_active_stories(dialog_ids.clone());
        let mut result = Vec::new();
        for dialog_id in &dialog_ids {
            for participant in &participants {
                if participant.dialog_id_ == *dialog_id {
                    result.push(participant.clone());
                    break;
                }
            }
        }
        promise.set_value(env.chat_members_object(total_count, result, "searchChatMembers"));
    }

    /// `DialogParticipantManager::get_channel_participants`
    /// (DialogParticipantManager.cpp:2033-2067) — the local gates, then the
    /// `GetChannelParticipantsQuery` send with the lot parking the promise
    /// and the lambda-carried filter/offset/limit/additional pair; the
    /// response machine is `on_get_channel_participants_answer`.
    #[allow(clippy::too_many_arguments)]
    pub fn get_channel_participants(
        &mut self,
        env: &mut impl DialogParticipantManagerEnv,
        channel_id: ChannelId,
        filter: &ChannelParticipantFilter,
        additional_query: String,
        offset: i32,
        limit: i32,
        additional_limit: i32,
        mut promise: Promise<td_api::ChatMembers>,
        source: &'static str,
    ) {
        let mut limit = limit;
        if limit <= 0 {
            return promise.set_error(Status::error(400, "Parameter limit must be positive"));
        }
        if limit > MAX_GET_CHANNEL_PARTICIPANTS {
            limit = MAX_GET_CHANNEL_PARTICIPANTS;
        }
        if offset < 0 {
            return promise.set_error(Status::error(400, "Parameter offset must be non-negative"));
        }

        if env.is_monoforum_channel(DialogId::from_channel_id(channel_id))
            || (env.is_broadcast_channel(channel_id)
                && !env.get_channel_status(channel_id).is_administrator())
        {
            return promise.set_error(Status::error(400, "Member list is inaccessible"));
        }

        // the C++ lambda parks the promise + captures before `->send`
        // resolves the input channel — settle the lot head on the send-leg
        // failure band (cpp:2053-2064, the promise error skips the intake)
        self.pending
            .channel_participants
            .push(PendingChannelParticipantsQuery {
                channel_id,
                filter: filter.clone(),
                offset,
                limit,
                additional_query,
                additional_limit,
                source,
                promise,
            });
        if let Err(error) =
            env.send_get_channel_participants_query(channel_id, filter, offset, limit)
        {
            let mut parked = self.pending.channel_participants.remove(0);
            parked.promise.set_error(error);
        }
    }

    // -- the net-drain 35 answer plane ------------------------------------

    /// `DialogParticipantManager::finish_get_channel_participant`
    /// (DialogParticipantManager.cpp:1917-1936) — the C++ lambda chain
    /// collapses here: the dialog-id match band, the restriction refresh,
    /// the cache add, then the C++ `finish_get_dialog_participant` tail
    /// (the min-user gate + the object conversion).
    fn finish_get_channel_participant(
        &mut self,
        env: &mut impl DialogParticipantManagerEnv,
        channel_id: ChannelId,
        participant_dialog_id: DialogId,
        mut dialog_participant: DialogParticipant,
        mut promise: Promise<td_api::ChatMember>,
    ) {
        let close_status = env.close_status();
        if close_status.is_error() {
            return promise.set_error(close_status);
        }

        if dialog_participant.dialog_id_ != participant_dialog_id {
            tdutils::log_error!(
                "Receive {:?} in {:?} instead of requested {:?}",
                dialog_participant.dialog_id_,
                channel_id,
                participant_dialog_id
            );
            return promise.set_error(Status::error(500, "Data is unavailable"));
        }

        tdutils::log_info!(
            "Receive {:?} as a member of a channel {:?}",
            dialog_participant.dialog_id_,
            channel_id
        );

        env.status_update_restrictions(&mut dialog_participant.status_);
        if self.have_channel_participant_cache(channel_id) {
            let cache_participant = dialog_participant.clone();
            self.add_channel_participant_to_cache(env, channel_id, cache_participant, false);
        }
        self.finish_get_dialog_participant(env, dialog_participant, promise);
    }

    /// The `GetChannelParticipantQuery` answer — the C++ handler
    /// `on_result`/`on_error` (cpp:365-394): the users/chats intakes, the
    /// participant build + validity band, the USER_NOT_PARTICIPANT
    /// left-participant value path, and the channel-error band (skipped
    /// when the participant dialog itself is a channel dialog).
    pub(crate) fn on_get_channel_participant_answer(
        &mut self,
        env: &mut impl DialogParticipantManagerEnv,
        result: Result<telegram_api::channels_ChannelParticipant, Status>,
    ) {
        if self.pending.channel_participant.is_empty() {
            return;
        }
        let (channel_id, participant_dialog_id, promise) =
            self.pending.channel_participant.remove(0);

        match result {
            Err(status) if status.message() == "USER_NOT_PARTICIPANT" => {
                // the left-participant value path rides the same finish
                // chain (cpp:385-388)
                let participant = DialogParticipant::left(participant_dialog_id);
                self.finish_get_channel_participant(
                    env,
                    channel_id,
                    participant_dialog_id,
                    participant,
                    promise,
                );
            }
            Err(status) => {
                let mut promise = promise;
                if participant_dialog_id.get_type() != DialogType::Channel {
                    env.on_get_channel_error(channel_id, &status, "GetChannelParticipantQuery");
                }
                promise.set_error(status);
            }
            Ok(answer) => {
                env.on_get_users(answer.users, "GetChannelParticipantQuery");
                env.on_get_chats(answer.chats, "GetChannelParticipantQuery");
                let mut promise = promise;
                let Some(participant) = answer.participant else {
                    return promise.set_error(Status::error(500, "Receive invalid chat member"));
                };
                let channel_type = env.get_channel_type(channel_id);
                let dialog_participant =
                    DialogParticipant::from_channel_participant(participant, channel_type);
                if !dialog_participant.is_valid() {
                    tdutils::log_error!("Receive invalid chat member");
                    return promise.set_error(Status::error(500, "Receive invalid chat member"));
                }
                self.finish_get_channel_participant(
                    env,
                    channel_id,
                    participant_dialog_id,
                    dialog_participant,
                    promise,
                );
            }
        }
    }

    /// The `GetChannelParticipantsQuery` answer — the C++ handler
    /// `on_result`/`on_error` (cpp:418-442) reentered into
    /// `on_get_channel_participants` (cpp:2069-2218): the filter
    /// re-check loop with the skip bands, the total-count fixups, the
    /// administrator/bot lists, the counts update, the additional-query
    /// search tail and the page conversion.
    pub(crate) fn on_get_channel_participants_answer(
        &mut self,
        env: &mut impl DialogParticipantManagerEnv,
        result: Result<telegram_api::channels_channelParticipants, Status>,
    ) {
        if self.pending.channel_participants.is_empty() {
            return;
        }
        let PendingChannelParticipantsQuery {
            channel_id,
            filter,
            offset,
            limit,
            additional_query,
            additional_limit,
            source,
            mut promise,
        } = self.pending.channel_participants.remove(0);

        // `TRY_STATUS_PROMISE(promise, G()->close_status())` (cpp:2073)
        let close_status = env.close_status();
        if close_status.is_error() {
            return promise.set_error(close_status);
        }

        let channel_participants = match result {
            Ok(participants) => participants,
            Err(status) => {
                env.on_get_channel_error(channel_id, &status, "GetChannelParticipantsQuery");
                return promise.set_error(status);
            }
        };

        env.on_get_users(channel_participants.users, "on_get_channel_participants");
        env.on_get_chats(channel_participants.chats, "on_get_channel_participants");
        let mut total_count = channel_participants.count;
        let participants = channel_participants.participants;
        tdutils::log_info!(
            "Receive {} members in {:?} with offset {} and limit {}",
            participants.len(),
            channel_id,
            offset,
            limit
        );

        let is_full = offset == 0 && (participants.len() as i32) < limit && total_count < limit;
        let has_hidden_participants = env.get_channel_effective_has_hidden_participants(
            channel_id,
            "on_get_channel_participants",
        );
        let is_full_recent = is_full && filter.is_recent() && !has_hidden_participants;

        let channel_type = env.get_channel_type(channel_id);
        let mut result: Vec<DialogParticipant> = Vec::with_capacity(participants.len());
        for participant in participants {
            let participant =
                DialogParticipant::from_channel_participant(participant, channel_type);
            let participant_user_id = if participant.dialog_id_.get_type() == DialogType::User {
                participant.dialog_id_.get_user_id()
            } else {
                UserId::default()
            };
            if !participant.is_valid()
                || (filter.is_bots() && !env.is_user_bot(participant_user_id))
                || (filter.is_administrators() && !participant.status_.is_administrator())
                || ((filter.is_recent() || filter.is_contacts() || filter.is_search())
                    && !participant.status_.is_member())
                || (filter.is_contacts() && !env.is_user_contact(participant_user_id))
                || (filter.is_restricted() && !participant.status_.is_restricted())
                || (filter.is_banned() && !participant.status_.is_banned())
            {
                let skip_error = ((filter.is_administrators() || filter.is_bots())
                    && env.is_user_deleted(participant_user_id))
                    || (filter.is_contacts() && participant_user_id == env.my_user_id());
                if !skip_error {
                    tdutils::log_error!(
                        "Receive an unsuitable member while searching in {:?} with offset {} and limit {}",
                        channel_id,
                        offset,
                        limit
                    );
                }
                total_count -= 1;
                continue;
            }
            result.push(participant);
        }

        if total_count < result.len() as i32 {
            tdutils::log_error!(
                "Receive total_count = {}, but have at least {} members in {:?}",
                total_count,
                result.len(),
                channel_id
            );
            total_count = result.len() as i32;
        } else if is_full && total_count > result.len() as i32 {
            tdutils::log_error!(
                "Fix total number of members from {} to {} in {:?} for request with limit {}",
                total_count,
                result.len(),
                channel_id,
                limit
            );
            total_count = result.len() as i32;
        }

        let is_megagroup = env.is_megagroup_channel(channel_id);
        let max_participant_count = if is_megagroup { 975 } else { 195 };
        let participant_count = if filter.is_recent()
            && !has_hidden_participants
            && total_count != 0
            && total_count < max_participant_count
        {
            total_count
        } else {
            -1
        };
        let mut administrator_count =
            if filter.is_administrators() || (filter.is_recent() && has_hidden_participants) {
                total_count
            } else {
                -1
            };
        if is_full && (filter.is_administrators() || filter.is_bots() || filter.is_recent()) {
            let mut administrators: Vec<DialogAdministrator> = Vec::new();
            let mut bot_user_ids: Vec<UserId> = Vec::new();
            if filter.is_recent() {
                for participant in &result {
                    if participant.dialog_id_.get_type() == DialogType::User {
                        let participant_user_id = participant.dialog_id_.get_user_id();
                        if participant.status_.is_administrator() {
                            administrators.push(DialogAdministrator::new(
                                participant_user_id,
                                participant.status_.get_rank().to_owned(),
                                participant.status_.is_creator(),
                                participant.status_.can_be_edited(),
                            ));
                        }
                        if is_full_recent && env.is_user_bot(participant_user_id) {
                            bot_user_ids.push(participant_user_id);
                        }
                    }
                }
                administrator_count = administrators.len() as i32;

                if is_megagroup && !env.auth_is_bot() && is_full_recent {
                    self.set_cached_channel_participants(channel_id, result.clone());
                    self.update_channel_online_member_count(env, channel_id, true);
                }
            } else if filter.is_administrators() {
                for participant in &result {
                    if participant.dialog_id_.get_type() == DialogType::User {
                        administrators.push(DialogAdministrator::new(
                            participant.dialog_id_.get_user_id(),
                            participant.status_.get_rank().to_owned(),
                            participant.status_.is_creator(),
                            participant.status_.can_be_edited(),
                        ));
                    }
                }
            } else if filter.is_bots() {
                // the C++ CHECKs the dialog type (the filter dropped every
                // non-user entry above)
                bot_user_ids = result
                    .iter()
                    .map(|participant| participant.dialog_id_.get_user_id())
                    .collect();
            }
            if filter.is_administrators() || filter.is_recent() {
                self.on_update_dialog_administrators(
                    env,
                    DialogId::from_channel_id(channel_id),
                    administrators,
                    true,
                    false,
                );
            }
            if filter.is_bots() || is_full_recent {
                env.on_update_channel_bot_user_ids(channel_id, bot_user_ids);
            }
        }
        if self.have_channel_participant_cache(channel_id) {
            for participant in result.clone() {
                self.add_channel_participant_to_cache(env, channel_id, participant, false);
            }
        }

        if participant_count != -1 {
            env.on_update_channel_participant_count(channel_id, participant_count);
        }
        if administrator_count != -1 {
            env.on_update_channel_administrator_count(channel_id, administrator_count);
        }

        if !additional_query.is_empty() {
            let dialog_ids: Vec<DialogId> = result.iter().map(|p| p.dialog_id_).collect();
            let (new_total_count, result_dialog_ids) =
                self.search_among_dialogs(env, &dialog_ids, &additional_query, additional_limit);

            total_count = new_total_count;
            let mut result_dialog_ids_set: std::collections::HashSet<i64> = result_dialog_ids
                .iter()
                .map(|dialog_id| dialog_id.get())
                .collect();
            let all_participants = std::mem::take(&mut result);
            for participant in all_participants {
                if result_dialog_ids_set.remove(&participant.dialog_id_.get()) {
                    result.push(participant);
                }
            }
        }

        let participant_dialog_ids: Vec<DialogId> = result
            .iter()
            .map(|participant| participant.dialog_id_)
            .collect();
        env.on_view_dialog_active_stories(participant_dialog_ids);

        promise.set_value(env.chat_members_object(total_count, result, source));
    }

    /// `DialogParticipantManager::set_cached_channel_participants`
    /// (DialogParticipantManager.cpp:3215-3218).
    fn set_cached_channel_participants(
        &mut self,
        channel_id: ChannelId,
        participants: Vec<DialogParticipant>,
    ) {
        self.cached_channel_participants
            .insert(channel_id.get(), participants);
    }

    /// `DialogParticipantManager::update_channel_online_member_count`
    /// (DialogParticipantManager.cpp:3222-3235).
    fn update_channel_online_member_count(
        &mut self,
        env: &mut impl DialogParticipantManagerEnv,
        channel_id: ChannelId,
        is_from_server: bool,
    ) {
        if !env.is_megagroup_channel(channel_id)
            || env.get_channel_effective_has_hidden_participants(
                channel_id,
                "update_channel_online_member_count",
            )
        {
            return;
        }

        let Some(participants) = self.cached_channel_participants.get(&channel_id.get()) else {
            return;
        };
        let participants = participants.clone();
        self.update_dialog_online_member_count_from_participants(
            env,
            &participants,
            DialogId::from_channel_id(channel_id),
            is_from_server,
        );
    }

    /// `DialogParticipantManager::update_dialog_online_member_count`'s
    /// vector form (cpp:1226-1260) — the online walker over a member page;
    /// the per-user seen-at map feeds the [blocked] refire timeout. (The
    /// C++ overloads share the cache-refresh form's name; the Rust pair
    /// can't.)
    fn update_dialog_online_member_count_from_participants(
        &mut self,
        env: &mut impl DialogParticipantManagerEnv,
        participants: &[DialogParticipant],
        dialog_id: DialogId,
        is_from_server: bool,
    ) {
        if env.auth_is_bot() {
            return;
        }

        let mut online_member_count = 0;
        let unix_time = env.unix_time();
        for participant in participants {
            if participant.dialog_id_.get_type() != DialogType::User {
                continue;
            }
            let user_id = participant.dialog_id_.get_user_id();
            if !env.is_user_deleted(user_id) && !env.is_user_bot(user_id) {
                if env.is_user_online(user_id, unix_time) {
                    online_member_count += 1;
                }
                if is_from_server {
                    let online_member_dialogs = self
                        .user_online_member_dialogs
                        .entry(user_id.get())
                        .or_default();
                    online_member_dialogs.insert(dialog_id.get(), unix_time);
                }
            }
        }
        self.on_update_dialog_online_member_count(
            env,
            dialog_id,
            online_member_count,
            is_from_server,
        );
    }

    /// `DialogParticipantManager::transfer_chat_ownership`
    /// (DialogParticipantManager.cpp:3392-3399) — the resolved-SRP tail of
    /// `transfer_dialog_ownership`: the close gate, then the
    /// `EditChatCreatorQuery` send with the lot parking the promise.
    pub fn transfer_chat_ownership(
        &mut self,
        env: &mut impl DialogParticipantManagerEnv,
        dialog_id: DialogId,
        user_id: UserId,
        input_check_password: telegram_api::InputCheckPasswordSRP,
        mut promise: Promise<()>,
    ) {
        if let status = env.close_status()
            && status.is_error()
        {
            return promise.set_error(status);
        }

        self.pending
            .edit_chat_creator
            .push((dialog_id, user_id, promise));
        if let Err(error) =
            env.send_edit_chat_creator_query(dialog_id, user_id, input_check_password)
        {
            let (_, _, mut promise) = self.pending.edit_chat_creator.remove(0);
            promise.set_error(error);
        }
    }

    /// The `EditChatCreatorQuery` answer — the C++ handler
    /// `on_result`/`on_error` (cpp:928-945): the dialog-error head, the
    /// channel-full invalidation, then the updates walk the promise rides
    /// (cpp:939) — returned to the drain, whose env view holds the
    /// updates manager.
    pub(crate) fn on_edit_chat_creator_answer(
        &mut self,
        env: &mut impl DialogParticipantManagerEnv,
        result: Result<telegram_api::Updates, Status>,
    ) -> Option<(telegram_api::Updates, Promise<()>)> {
        if self.pending.edit_chat_creator.is_empty() {
            return None;
        }
        let (dialog_id, _, mut promise) = self.pending.edit_chat_creator.remove(0);

        let updates = match result {
            Ok(updates) => updates,
            Err(status) => {
                env.on_get_dialog_error(dialog_id, &status, "EditChatCreatorQuery");
                promise.set_error(status);
                return None;
            }
        };
        tdutils::log_info!("Receive result for EditChatCreatorQuery");
        if dialog_id.get_type() == DialogType::Channel {
            env.invalidate_channel_full(dialog_id.get_channel_id(), false, "EditChatCreatorQuery");
        }
        Some((updates, promise))
    }

    /// `DialogParticipantManager::search_dialog_participants`
    /// (DialogParticipantManager.cpp:2220-2256).
    pub fn search_dialog_participants(
        &mut self,
        env: &mut impl DialogParticipantManagerEnv,
        dialog_id: DialogId,
        query: &str,
        limit: i32,
        filter: &DialogParticipantFilter,
        mut promise: Promise<td_api::ChatMembers>,
    ) {
        if !env.have_dialog_force(dialog_id, "search_dialog_participants") {
            return promise.set_error(Status::error(400, "Chat not found"));
        }
        if limit < 0 {
            return promise.set_error(Status::error(400, "Parameter limit must be non-negative"));
        }

        match dialog_id.get_type() {
            DialogType::User => {
                let participants = self.search_private_chat_participants(
                    env,
                    dialog_id.get_user_id(),
                    query,
                    limit,
                    filter,
                );
                promise.set_value(env.chat_members_object(
                    participants.total_count,
                    participants.participants,
                    "searchChatMembers",
                ));
            }
            DialogType::Chat => {
                self.search_chat_participants(
                    env,
                    dialog_id.get_chat_id(),
                    query,
                    limit,
                    filter,
                    promise,
                );
            }
            DialogType::Channel => {
                let channel_id = dialog_id.get_channel_id();
                if filter.has_query() {
                    // the user query rides the server-side filter; no
                    // additional pass (cpp:2239-2241)
                    self.get_channel_participants(
                        env,
                        channel_id,
                        &filter.as_channel_participant_filter(query),
                        String::new(),
                        0,
                        limit,
                        0,
                        promise,
                        "searchChatMembers",
                    );
                } else {
                    // the user query searches the fixed 100-member first
                    // page client-side (cpp:2243-2244)
                    self.get_channel_participants(
                        env,
                        channel_id,
                        &filter.as_channel_participant_filter(""),
                        query.to_owned(),
                        0,
                        100,
                        limit,
                        promise,
                        "searchChatMembers",
                    );
                }
            }
            DialogType::SecretChat => {
                let peer_user_id = env.get_secret_chat_user_id(dialog_id);
                let participants =
                    self.search_private_chat_participants(env, peer_user_id, query, limit, filter);
                promise.set_value(env.chat_members_object(
                    participants.total_count,
                    participants.participants,
                    "searchChatMembers",
                ));
            }
            DialogType::None => unreachable!(),
        }
    }

    // -- the join / mutation plane ----------------------------------------

    /// `DialogParticipantManager::join_dialog` (DialogParticipantManager.cpp:2258-2277).
    pub fn join_dialog(
        &mut self,
        env: &mut impl DialogParticipantManagerEnv,
        dialog_id: DialogId,
        mut promise: Promise<td_api::ChatJoinResult>,
    ) {
        if !env.have_dialog_force(dialog_id, "join_dialog") {
            return promise.set_error(Status::error(400, "Chat not found"));
        }
        match dialog_id.get_type() {
            DialogType::User => promise.set_error(Status::error(400, "Can't join private chats")),
            DialogType::Chat => self.join_chat(env, dialog_id.get_chat_id(), promise),
            DialogType::Channel => self.join_channel(env, dialog_id.get_channel_id(), promise),
            DialogType::SecretChat => {
                promise.set_error(Status::error(400, "Can't join secret chats"))
            }
            DialogType::None => unreachable!(),
        }
    }

    /// `DialogParticipantManager::join_chat` (DialogParticipantManager.cpp:2457+)
    /// — the local gates; the invite-acceptance/add wire send is the seam.
    fn join_chat(
        &mut self,
        env: &mut impl DialogParticipantManagerEnv,
        chat_id: ChatId,
        mut promise: Promise<td_api::ChatJoinResult>,
    ) {
        if !env.get_chat_is_active(chat_id) {
            return promise.set_error(Status::error(400, "Chat is deactivated"));
        }
        if env.get_chat_status(chat_id).is_banned() {
            return promise.set_error(Status::error(400, "User is banned from the chat"));
        }
        let my_user_id = env.my_user_id();
        env.join_or_add_chat_member_query(chat_id, my_user_id);
    }

    /// `DialogParticipantManager::join_channel` — the local gate + the
    /// pending-query machinery collapses into the seam.
    fn join_channel(
        &mut self,
        env: &mut impl DialogParticipantManagerEnv,
        channel_id: ChannelId,
        mut promise: Promise<td_api::ChatJoinResult>,
    ) {
        let status = env.get_channel_status(channel_id);
        if !status.is_member() && status.is_banned() {
            return promise.set_error(Status::error(400, "User is banned from the chat"));
        }
        env.join_channel_query(channel_id);
    }

    /// `DialogParticipantManager::add_dialog_participant`
    /// (DialogParticipantManager.cpp:2284-2305).
    pub fn add_dialog_participant(
        &mut self,
        env: &mut impl DialogParticipantManagerEnv,
        dialog_id: DialogId,
        user_id: UserId,
        _forward_limit: i32,
        mut promise: Promise<()>,
    ) {
        if !env.have_dialog_force(dialog_id, "add_dialog_participant") {
            return promise.set_error(Status::error(400, "Chat not found"));
        }
        match dialog_id.get_type() {
            DialogType::User => {
                promise.set_error(Status::error(400, "Can't add members to a private chat"))
            }
            DialogType::Chat => {
                env.join_or_add_chat_member_query(dialog_id.get_chat_id(), user_id);
            }
            DialogType::Channel => {
                env.invite_to_channel_query(dialog_id.get_channel_id(), user_id);
            }
            DialogType::SecretChat => {
                promise.set_error(Status::error(400, "Can't add members to a secret chat"))
            }
            DialogType::None => unreachable!(),
        }
    }

    /// `DialogParticipantManager::add_dialog_participants`
    /// (DialogParticipantManager.cpp:2307-2329).
    pub fn add_dialog_participants(
        &mut self,
        env: &mut impl DialogParticipantManagerEnv,
        dialog_id: DialogId,
        user_ids: Vec<UserId>,
        mut promise: Promise<()>,
    ) {
        if !env.have_dialog_force(dialog_id, "add_dialog_participants") {
            return promise.set_error(Status::error(400, "Chat not found"));
        }
        match dialog_id.get_type() {
            DialogType::User => {
                promise.set_error(Status::error(400, "Can't add members to a private chat"))
            }
            DialogType::Chat => {
                if user_ids.len() == 1 {
                    env.join_or_add_chat_member_query(dialog_id.get_chat_id(), user_ids[0]);
                    return;
                }
                promise.set_error(Status::error(
                    400,
                    "Can't add many members at once to a basic group chat",
                ))
            }
            DialogType::Channel => {
                for user_id in user_ids {
                    env.invite_to_channel_query(dialog_id.get_channel_id(), user_id);
                }
            }
            DialogType::SecretChat => {
                promise.set_error(Status::error(400, "Can't add members to a secret chat"))
            }
            DialogType::None => unreachable!(),
        }
    }

    /// `DialogParticipantManager::set_dialog_participant_status`
    /// (DialogParticipantManager.cpp:2331-2362).
    pub fn set_dialog_participant_status(
        &mut self,
        env: &mut impl DialogParticipantManagerEnv,
        dialog_id: DialogId,
        participant_dialog_id: DialogId,
        chat_member_status: td_api::ChatMemberStatus,
        mut promise: Promise<()>,
    ) {
        if !env.have_dialog_force(dialog_id, "set_dialog_participant_status") {
            return promise.set_error(Status::error(400, "Chat not found"));
        }
        match dialog_id.get_type() {
            DialogType::User => promise.set_error(Status::error(
                400,
                "Chat member status can't be changed in private chats",
            )),
            DialogType::Chat => {
                let status = get_dialog_participant_status(
                    env,
                    Some(&chat_member_status),
                    ChannelType::Unknown,
                );
                if participant_dialog_id.get_type() != DialogType::User {
                    if status == DialogParticipantStatus::left() {
                        return promise.set_value(());
                    }
                    return promise
                        .set_error(Status::error(400, "Chats can't be members of basic groups"));
                }
                env.edit_chat_participant_query(
                    dialog_id.get_chat_id(),
                    participant_dialog_id.get_user_id(),
                    status,
                );
            }
            DialogType::Channel => {
                let status = get_dialog_participant_status(
                    env,
                    Some(&chat_member_status),
                    ChannelType::Unknown,
                );
                env.edit_channel_participant_query(
                    dialog_id.get_channel_id(),
                    participant_dialog_id,
                    status,
                );
            }
            DialogType::SecretChat => promise.set_error(Status::error(
                400,
                "Chat member status can't be changed in secret chats",
            )),
            DialogType::None => unreachable!(),
        }
    }

    /// `DialogParticipantManager::set_dialog_participant_rank`
    /// (DialogParticipantManager.cpp:2364-2383).
    pub fn set_dialog_participant_rank(
        &mut self,
        env: &mut impl DialogParticipantManagerEnv,
        dialog_id: DialogId,
        user_id: UserId,
        rank: String,
        mut promise: Promise<()>,
    ) {
        if let Err(error) = env.check_dialog_access_read(dialog_id, "set_dialog_participant_rank 1")
        {
            return promise.set_error(error);
        }
        match dialog_id.get_type() {
            DialogType::User => promise.set_error(Status::error(
                400,
                "Chat member tag can't be changed in private chats",
            )),
            DialogType::Chat | DialogType::Channel => {
                self.speculative_update_dialog_administrator_rank(env, dialog_id, user_id, &rank);
                env.edit_chat_participant_rank_query(dialog_id, user_id, rank);
            }
            DialogType::SecretChat | DialogType::None => unreachable!(),
        }
    }

    /// `DialogParticipantManager::ban_dialog_participant`
    /// (DialogParticipantManager.cpp:2385-2412).
    pub fn ban_dialog_participant(
        &mut self,
        env: &mut impl DialogParticipantManagerEnv,
        dialog_id: DialogId,
        participant_dialog_id: DialogId,
        banned_until_date: i32,
        _revoke_messages: bool,
        mut promise: Promise<()>,
    ) {
        if !env.have_dialog_force(dialog_id, "ban_dialog_participant") {
            return promise.set_error(Status::error(400, "Chat not found"));
        }
        match dialog_id.get_type() {
            DialogType::User => {
                promise.set_error(Status::error(400, "Can't ban members in private chats"))
            }
            DialogType::Chat => {
                if participant_dialog_id.get_type() != DialogType::User {
                    return promise
                        .set_error(Status::error(400, "Can't ban chats in basic groups"));
                }
                // `delete_chat_participant(chat_id, user_id, revoke_messages)`
                // — the messages.deleteChatUser wire send is the seam
                env.edit_chat_participant_query(
                    dialog_id.get_chat_id(),
                    participant_dialog_id.get_user_id(),
                    DialogParticipantStatus::banned(banned_until_date, String::new()),
                );
            }
            DialogType::Channel => {
                // must use chatMemberStatusBanned to properly fix banned_until_date
                env.edit_channel_participant_query(
                    dialog_id.get_channel_id(),
                    participant_dialog_id,
                    DialogParticipantStatus::banned(banned_until_date, String::new()),
                );
            }
            DialogType::SecretChat => {
                promise.set_error(Status::error(400, "Can't ban members in secret chats"))
            }
            DialogType::None => unreachable!(),
        }
    }

    /// `DialogParticipantManager::leave_dialog` (DialogParticipantManager.cpp:2414-2439).
    pub fn leave_dialog(
        &mut self,
        env: &mut impl DialogParticipantManagerEnv,
        dialog_id: DialogId,
        mut promise: Promise<()>,
    ) {
        if !env.have_dialog_force(dialog_id, "leave_dialog") {
            return promise.set_error(Status::error(400, "Chat not found"));
        }
        match dialog_id.get_type() {
            DialogType::User => promise.set_error(Status::error(400, "Can't leave private chats")),
            DialogType::Chat => {
                // `delete_chat_participant(chat_id, my_user_id, false)`
                let my_user_id = env.my_user_id();
                env.edit_chat_participant_query(
                    dialog_id.get_chat_id(),
                    my_user_id,
                    DialogParticipantStatus::left(),
                );
            }
            DialogType::Channel => {
                let channel_id = dialog_id.get_channel_id();
                let mut new_status = env.get_channel_status(channel_id);
                new_status.set_is_member(false);
                let my_dialog_id = env.my_dialog_id();
                env.edit_channel_participant_query(channel_id, my_dialog_id, new_status);
            }
            DialogType::SecretChat => {
                promise.set_error(Status::error(400, "Can't leave secret chats"))
            }
            DialogType::None => unreachable!(),
        }
    }

    // -- the join-request plane -------------------------------------------

    /// `DialogParticipantManager::can_manage_dialog_join_requests`
    /// (DialogParticipantManager.cpp:1256-1284).
    pub fn can_manage_dialog_join_requests(
        &mut self,
        env: &mut impl DialogParticipantManagerEnv,
        dialog_id: DialogId,
    ) -> Result<(), Status> {
        env.check_dialog_access_write(dialog_id, "can_manage_dialog_join_requests")?;

        match dialog_id.get_type() {
            DialogType::SecretChat | DialogType::User => {
                Err(Status::error(400, "The chat can't have join requests"))
            }
            DialogType::Chat => {
                let chat_id = dialog_id.get_chat_id();
                if !env.get_chat_is_active(chat_id) {
                    return Err(Status::error(400, "Chat is deactivated"));
                }
                if !env.get_chat_status(chat_id).can_manage_invite_links() {
                    return Err(Status::error(
                        400,
                        "Not enough rights to manage chat join requests",
                    ));
                }
                Ok(())
            }
            DialogType::Channel => {
                if !env
                    .get_channel_status(dialog_id.get_channel_id())
                    .can_manage_invite_links()
                {
                    return Err(Status::error(
                        400,
                        "Not enough rights to manage chat join requests",
                    ));
                }
                Ok(())
            }
            DialogType::None => unreachable!(),
        }
    }

    /// `DialogParticipantManager::fix_pending_join_requests`
    /// (DialogParticipantManager.cpp:1286-1331).
    pub fn fix_pending_join_requests(
        &mut self,
        env: &mut impl DialogParticipantManagerEnv,
        dialog_id: DialogId,
        pending_join_request_count: &mut i32,
        pending_join_request_user_ids: &mut Vec<UserId>,
    ) {
        let need_drop_pending_join_requests = if *pending_join_request_count < 0 {
            true
        } else {
            match dialog_id.get_type() {
                DialogType::User | DialogType::SecretChat => true,
                DialogType::Chat => !env
                    .get_chat_status(dialog_id.get_chat_id())
                    .can_manage_invite_links(),
                DialogType::Channel => !env
                    .get_channel_permissions(dialog_id.get_channel_id())
                    .can_manage_invite_links(),
                DialogType::None => unreachable!(),
            }
        };
        if need_drop_pending_join_requests {
            *pending_join_request_count = 0;
            pending_join_request_user_ids.clear();
        } else if (*pending_join_request_count as usize) < pending_join_request_user_ids.len() {
            *pending_join_request_count = pending_join_request_user_ids.len() as i32;
        }

        if pending_join_request_user_ids.len() > MAX_PENDING_JOIN_REQUESTS {
            pending_join_request_user_ids.truncate(MAX_PENDING_JOIN_REQUESTS);
        }
    }

    /// `DialogParticipantManager::get_dialog_join_requests`
    /// (DialogParticipantManager.cpp:1333-1352).
    #[allow(clippy::too_many_arguments)] // the C++ signature
    pub fn get_dialog_join_requests(
        &mut self,
        env: &mut impl DialogParticipantManagerEnv,
        dialog_id: DialogId,
        invite_link: String,
        query: String,
        offset_request: Option<td_api::ChatJoinRequest>,
        limit: i32,
        mut promise: Promise<td_api::ChatJoinRequests>,
    ) {
        if let Err(error) = self.can_manage_dialog_join_requests(env, dialog_id) {
            return promise.set_error(error);
        }
        if limit <= 0 {
            return promise.set_error(Status::error(400, "Parameter limit must be positive"));
        }

        let (offset_user_id, offset_date) = match &offset_request {
            Some(request) => (UserId::new(request.user_id), request.date),
            None => (UserId::new(0), 0),
        };

        env.get_chat_join_requests_query(
            dialog_id,
            invite_link,
            query,
            offset_date,
            offset_user_id,
            limit,
        );
    }

    /// `DialogParticipantManager::process_dialog_join_request`
    /// (DialogParticipantManager.cpp:1360-1364).
    pub fn process_dialog_join_request(
        &mut self,
        env: &mut impl DialogParticipantManagerEnv,
        dialog_id: DialogId,
        user_id: UserId,
        approve: bool,
        mut promise: Promise<()>,
    ) {
        if let Err(error) = self.can_manage_dialog_join_requests(env, dialog_id) {
            return promise.set_error(error);
        }
        env.hide_chat_join_request_query(dialog_id, user_id, approve);
    }

    /// `DialogParticipantManager::process_dialog_join_requests`
    /// (DialogParticipantManager.cpp:1366-1370).
    pub fn process_dialog_join_requests(
        &mut self,
        env: &mut impl DialogParticipantManagerEnv,
        dialog_id: DialogId,
        invite_link: String,
        approve: bool,
        mut promise: Promise<()>,
    ) {
        if let Err(error) = self.can_manage_dialog_join_requests(env, dialog_id) {
            return promise.set_error(error);
        }
        env.hide_all_chat_join_requests_query(dialog_id, invite_link, approve);
    }

    /// `DialogParticipantManager::set_join_chat_result`
    /// (DialogParticipantManager.cpp:1354-1358) — the
    /// answerChatJoinRequestQuery arm (CHECK_IS_BOT at the dispatcher).
    pub fn set_join_chat_result(
        &mut self,
        env: &mut impl DialogParticipantManagerEnv,
        query_id: i64,
        result: td_api::ChatJoinRequestResult,
        url: String,
    ) {
        env.set_join_chat_results_query(query_id, result, url);
    }

    /// `DialogParticipantManager::get_chat_join_web_view_url`
    /// (DialogParticipantManager.cpp:2279-2282) — the getGuardBotWebAppUrl arm.
    pub fn get_chat_join_web_view_url(
        &mut self,
        env: &mut impl DialogParticipantManagerEnv,
        query_id: i64,
        _promise: Promise<td_api::WebAppUrl>,
    ) {
        env.request_chat_join_web_view_query(query_id);
    }

    // -- the ownership-transfer plane -------------------------------------

    /// `DialogParticipantManager::can_transfer_ownership`'s error mapping
    /// (DialogParticipantManager.cpp:3301-3330) — pure, C++-anchored.
    pub fn map_can_transfer_ownership_error(
        error: &Status,
    ) -> Result<CanTransferOwnershipResult, Status> {
        let message = error.message().to_string();
        if message == "PASSWORD_HASH_INVALID" {
            return Ok(CanTransferOwnershipResult::default());
        }
        if message == "PASSWORD_MISSING" {
            return Ok(CanTransferOwnershipResult {
                result_type: CanTransferOwnershipResultType::PasswordNeeded,
                retry_after: 0,
            });
        }
        if let Some(rest) = message.strip_prefix("PASSWORD_TOO_FRESH_") {
            let mut retry_after: i32 = rest.parse().unwrap_or(0);
            if retry_after < 0 {
                retry_after = 0;
            }
            return Ok(CanTransferOwnershipResult {
                result_type: CanTransferOwnershipResultType::PasswordTooFresh,
                retry_after,
            });
        }
        if let Some(rest) = message.strip_prefix("SESSION_TOO_FRESH_") {
            let mut retry_after: i32 = rest.parse().unwrap_or(0);
            if retry_after < 0 {
                retry_after = 0;
            }
            return Ok(CanTransferOwnershipResult {
                result_type: CanTransferOwnershipResultType::SessionTooFresh,
                retry_after,
            });
        }
        Err(error.clone())
    }

    /// `DialogParticipantManager::can_transfer_ownership`
    /// (DialogParticipantManager.cpp:3300-3333) — the query seam parks; the
    /// mapping lives in [`Self::map_can_transfer_ownership_error`].
    pub fn can_transfer_ownership(&mut self, env: &mut impl DialogParticipantManagerEnv) {
        env.can_edit_chat_creator_query();
    }

    /// `DialogParticipantManager::get_can_transfer_ownership_result_object`
    /// (DialogParticipantManager.cpp:3335-3350).
    pub fn get_can_transfer_ownership_result_object(
        result: CanTransferOwnershipResult,
    ) -> td_api::CanTransferOwnershipResult {
        match result.result_type {
            CanTransferOwnershipResultType::Ok => {
                td_api::CanTransferOwnershipResult::canTransferOwnershipResultOk(
                    td_api::canTransferOwnershipResultOk {},
                )
            }
            CanTransferOwnershipResultType::PasswordNeeded => {
                td_api::CanTransferOwnershipResult::canTransferOwnershipResultPasswordNeeded(
                    td_api::canTransferOwnershipResultPasswordNeeded {},
                )
            }
            CanTransferOwnershipResultType::PasswordTooFresh => {
                td_api::CanTransferOwnershipResult::canTransferOwnershipResultPasswordTooFresh(
                    td_api::canTransferOwnershipResultPasswordTooFresh {
                        retry_after: result.retry_after,
                    },
                )
            }
            CanTransferOwnershipResultType::SessionTooFresh => {
                td_api::CanTransferOwnershipResult::canTransferOwnershipResultSessionTooFresh(
                    td_api::canTransferOwnershipResultSessionTooFresh {
                        retry_after: result.retry_after,
                    },
                )
            }
        }
    }

    /// `DialogParticipantManager::transfer_dialog_ownership`
    /// (DialogParticipantManager.cpp:3352-3390) — the local gates; the SRP
    /// half is the [blocked: PasswordManager] seam and the promise parks in
    /// the `EditChatCreatorQuery` lot until the SRP resolves (the C++
    /// request awaiting its own net answer). The resolved-SRP tail is
    /// [`DialogParticipantManager::transfer_chat_ownership`].
    pub fn transfer_dialog_ownership(
        &mut self,
        env: &mut impl DialogParticipantManagerEnv,
        dialog_id: DialogId,
        user_id: UserId,
        password: &str,
        mut promise: Promise<()>,
    ) {
        if let Err(error) = env.check_dialog_access_read(dialog_id, "transfer_dialog_ownership") {
            return promise.set_error(error);
        }
        if !env.have_user_force(user_id, "transfer_dialog_ownership") {
            return promise.set_error(Status::error(400, "User not found"));
        }
        if env.is_user_bot(user_id) {
            return promise.set_error(Status::error(400, "User is a bot"));
        }
        if env.is_user_deleted(user_id) {
            return promise.set_error(Status::error(400, "User is deleted"));
        }
        if password.is_empty() {
            return promise.set_error(Status::error(400, "PASSWORD_HASH_INVALID"));
        }

        match dialog_id.get_type() {
            DialogType::User | DialogType::SecretChat => {
                promise.set_error(Status::error(400, "Can't transfer chat ownership"))
            }
            DialogType::Chat | DialogType::Channel => {
                // the promise parks at the SRP seam — its own lot, never
                // paired with the wire answers
                self.pending
                    .transfer_ownership_srp
                    .push((dialog_id, user_id, promise));
                env.transfer_chat_ownership_seam(dialog_id, user_id, password.to_owned());
            }
            DialogType::None => unreachable!(),
        }
    }

    /// `DialogParticipantManager::get_future_creator_after_leave`
    /// (DialogParticipantManager.cpp:3401-3410).
    pub fn get_future_creator_after_leave(
        &mut self,
        env: &mut impl DialogParticipantManagerEnv,
        dialog_id: DialogId,
        mut promise: Promise<td_api::User>,
    ) {
        if let Err(error) =
            env.check_dialog_access_write(dialog_id, "get_future_creator_after_leave")
        {
            return promise.set_error(error);
        }
        if !env.get_dialog_status(dialog_id).is_creator() {
            // the C++ sets a nullptr user
            return env.send_null_user(promise);
        }
        env.get_future_chat_creator_query(dialog_id);
    }

    // -- the channel participant cache ------------------------------------

    /// `DialogParticipantManager::have_channel_participant_cache`
    /// (DialogParticipantManager.cpp:3145-3150).
    pub fn have_channel_participant_cache(&self, channel_id: ChannelId) -> bool {
        self.channel_participants.contains_key(&channel_id.get())
    }

    /// `DialogParticipantManager::add_channel_participant_to_cache`
    /// (DialogParticipantManager.cpp:3152-3167).
    pub fn add_channel_participant_to_cache(
        &mut self,
        env: &mut impl DialogParticipantManagerEnv,
        channel_id: ChannelId,
        dialog_participant: DialogParticipant,
        allow_replace: bool,
    ) {
        let unix_time = env.unix_time();
        let entry = self
            .channel_participants
            .entry(channel_id.get())
            .or_default();
        match entry.get_mut(&dialog_participant.dialog_id_.get()) {
            Some(existing) => {
                if !allow_replace {
                    return;
                }
                *existing = (dialog_participant, unix_time);
            }
            None => {
                entry.insert(
                    dialog_participant.dialog_id_.get(),
                    (dialog_participant, unix_time),
                );
            }
        }
        if self.channel_participant_cache_deadlines.is_empty() {
            let timeout = CHANNEL_PARTICIPANT_CACHE_TIME as f64;
            self.channel_participant_cache_deadlines
                .insert(channel_id.get(), env.now() + timeout);
            env.set_channel_participant_cache_timeout(channel_id, timeout);
        }
    }

    /// `DialogParticipantManager::get_channel_participant_from_cache`
    /// (DialogParticipantManager.cpp:3194-3200).
    pub fn get_channel_participant_from_cache(
        &self,
        channel_id: ChannelId,
        participant_dialog_id: DialogId,
    ) -> Option<&DialogParticipant> {
        self.channel_participants
            .get(&channel_id.get())?
            .get(&participant_dialog_id.get())
            .map(|(participant, _)| participant)
    }

    /// `DialogParticipantManager::update_channel_participant_status_cache`
    /// (DialogParticipantManager.cpp:3169-3188) — the
    /// `status_.update_restrictions()` call rides the env's Global.
    pub fn update_channel_participant_status_cache(
        &mut self,
        env: &mut impl DialogParticipantManagerEnv,
        channel_id: ChannelId,
        participant_dialog_id: DialogId,
        dialog_participant_status: DialogParticipantStatus,
    ) {
        let unix_time = env.unix_time();
        let Some(entry) = self.channel_participants.get_mut(&channel_id.get()) else {
            return;
        };
        match entry.get_mut(&participant_dialog_id.get()) {
            Some((participant, _)) => {
                participant.status_ = dialog_participant_status;
                env.status_update_restrictions(&mut participant.status_);
            }
            None => {
                let dialog_participant = DialogParticipant::new(
                    participant_dialog_id,
                    UserId::new(0),
                    unix_time,
                    dialog_participant_status,
                );
                entry.insert(participant_dialog_id.get(), (dialog_participant, unix_time));
            }
        }
    }

    /// `DialogParticipantManager::drop_channel_participant_cache`
    /// (DialogParticipantManager.cpp:3190-3192).
    pub fn drop_channel_participant_cache(&mut self, channel_id: ChannelId) {
        self.channel_participants.remove(&channel_id.get());
        self.channel_participant_cache_deadlines
            .remove(&channel_id.get());
    }

    // -- the current-state plane ------------------------------------------

    /// `DialogParticipantManager::get_current_state`
    /// (DialogParticipantManager.cpp:3412-3421).
    pub fn get_current_state(
        &self,
        env: &mut impl DialogParticipantManagerEnv,
        updates: &mut Vec<td_api::Update>,
    ) {
        for (&dialog_id_key, info) in &self.dialog_online_member_counts {
            let dialog_id = DialogId::new(dialog_id_key);
            if info.is_update_sent && env.is_dialog_opened(dialog_id) {
                updates.push(td_api::Update::updateChatOnlineMemberCount(
                    td_api::updateChatOnlineMemberCount {
                        chat_id: env.get_chat_id_object(dialog_id, "updateChatOnlineMemberCount"),
                        online_member_count: info.online_member_count,
                    },
                ));
            }
        }
    }
}

/// `DialogParticipantManager::CanTransferOwnershipResult`
/// (DialogParticipantManager.h:152-156).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CanTransferOwnershipResult {
    pub result_type: CanTransferOwnershipResultType,
    pub retry_after: i32,
}

/// the `Type` enum of [`CanTransferOwnershipResult`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CanTransferOwnershipResultType {
    #[default]
    Ok,
    PasswordNeeded,
    PasswordTooFresh,
    SessionTooFresh,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn can_transfer_ownership_error_mapping() {
        let result = DialogParticipantManager::map_can_transfer_ownership_error(&Status::error(
            400,
            "PASSWORD_HASH_INVALID",
        ))
        .unwrap();
        assert_eq!(result.result_type, CanTransferOwnershipResultType::Ok);

        let result = DialogParticipantManager::map_can_transfer_ownership_error(&Status::error(
            400,
            "PASSWORD_MISSING",
        ))
        .unwrap();
        assert_eq!(
            result.result_type,
            CanTransferOwnershipResultType::PasswordNeeded
        );
        assert_eq!(result.retry_after, 0);

        let result = DialogParticipantManager::map_can_transfer_ownership_error(&Status::error(
            400,
            "PASSWORD_TOO_FRESH_1234",
        ))
        .unwrap();
        assert_eq!(
            result.result_type,
            CanTransferOwnershipResultType::PasswordTooFresh
        );
        assert_eq!(result.retry_after, 1234);

        let result = DialogParticipantManager::map_can_transfer_ownership_error(&Status::error(
            400,
            "SESSION_TOO_FRESH_-7",
        ))
        .unwrap();
        assert_eq!(
            result.result_type,
            CanTransferOwnershipResultType::SessionTooFresh
        );
        // the C++ clamps a negative retry_after to 0
        assert_eq!(result.retry_after, 0);

        assert!(
            DialogParticipantManager::map_can_transfer_ownership_error(&Status::error(
                429,
                "TELEGRAM_SERVER_ERROR"
            ))
            .is_err()
        );
    }

    #[test]
    fn object_construction_per_result_type() {
        let object = DialogParticipantManager::get_can_transfer_ownership_result_object(
            CanTransferOwnershipResult::default(),
        );
        assert!(matches!(
            object,
            td_api::CanTransferOwnershipResult::canTransferOwnershipResultOk(_)
        ));
        let object = DialogParticipantManager::get_can_transfer_ownership_result_object(
            CanTransferOwnershipResult {
                result_type: CanTransferOwnershipResultType::PasswordTooFresh,
                retry_after: 33,
            },
        );
        let td_api::CanTransferOwnershipResult::canTransferOwnershipResultPasswordTooFresh(inner) =
            object
        else {
            panic!("wrong variant");
        };
        assert_eq!(inner.retry_after, 33);
    }

    #[test]
    fn administrators_database_key() {
        // PSTRING() << "adm" << -dialog_id.get()
        let dialog_id = DialogId::new(-1_000_000_000_001); // a channel dialog
        assert_eq!(
            DialogParticipantManager::get_dialog_administrators_database_key(dialog_id),
            b"adm1000000000001".to_vec()
        );
    }

    #[test]
    fn administrator_vector_round_trips() {
        let administrators = vec![
            DialogAdministrator::new(UserId::new(5), String::new(), false, true),
            DialogAdministrator::new(UserId::new(2), "owner".to_owned(), true, false),
        ];
        let mut out = Vec::new();
        store_dialog_administrators(&administrators, &mut out);
        let mut parser = Parser::new(&out);
        let parsed = parse_dialog_administrators(&mut parser).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[1].get_user_id(), UserId::new(2));
        assert_eq!(parsed[1].get_rank(), "owner");
        assert!(parsed[1].is_creator());
    }

    // -- the net-drain 35 band tests ---------------------------------------

    /// The band-test env: records the send arms (with a configurable
    /// failure) and answers the finish-chain probes from a tiny user
    /// registry. Everything else answers the C++ no-op defaults.
    struct BandEnv {
        users: Vec<i64>,
        send_errors: Vec<Status>,
        sent: Vec<&'static str>,
        get_participant_peers: Vec<DialogId>,
        get_participants_queries: Vec<(ChannelId, i32, i32)>,
        edit_creator_queries: Vec<(DialogId, UserId)>,
        channel_errors: Vec<Status>,
    }

    impl BandEnv {
        fn new(users: &[i64]) -> BandEnv {
            BandEnv {
                users: users.to_vec(),
                send_errors: Vec::new(),
                sent: Vec::new(),
                get_participant_peers: Vec::new(),
                get_participants_queries: Vec::new(),
                edit_creator_queries: Vec::new(),
                channel_errors: Vec::new(),
            }
        }

        fn fail_next_send_with(&mut self, error: Status) {
            self.send_errors.push(error);
        }
    }

    impl crate::dialog_participant_filter::DialogParticipantFilterEnv for BandEnv {
        fn is_user_contact(&self, user_id: UserId) -> bool {
            self.users.contains(&user_id.get())
        }

        fn is_user_bot(&self, _user_id: UserId) -> bool {
            // nobody is a bot here — the transfer gate ("User is a bot")
            // and the bots-filter drop band both read this
            false
        }
    }

    /// The settled-lot recorder.
    #[derive(Clone, Default)]
    struct RecMember {
        settled: Arc<std::sync::Mutex<Vec<Result<(), Status>>>>,
    }

    impl tdutils::promise::PromiseInterface<td_api::ChatMember> for RecMember {
        fn set_value(&mut self, _: td_api::ChatMember) {
            self.settled.lock().unwrap().push(Ok(()));
        }

        fn set_error(&mut self, error: Status) {
            self.settled.lock().unwrap().push(Err(error));
        }
    }

    #[derive(Clone, Default)]
    struct RecMembers {
        settled: Arc<std::sync::Mutex<Vec<Result<(), Status>>>>,
    }

    impl tdutils::promise::PromiseInterface<td_api::ChatMembers> for RecMembers {
        fn set_value(&mut self, _: td_api::ChatMembers) {
            self.settled.lock().unwrap().push(Ok(()));
        }

        fn set_error(&mut self, error: Status) {
            self.settled.lock().unwrap().push(Err(error));
        }
    }

    /// The settled-`Result<()>` recorder — the transfer promise tail.
    #[derive(Clone, Default)]
    struct RecUnit {
        settled: Arc<std::sync::Mutex<Vec<Result<(), Status>>>>,
    }

    impl tdutils::promise::PromiseInterface<()> for RecUnit {
        fn set_value(&mut self, _: ()) {
            self.settled.lock().unwrap().push(Ok(()));
        }

        fn set_error(&mut self, error: Status) {
            self.settled.lock().unwrap().push(Err(error));
        }
    }

    impl DialogParticipantManagerEnv for BandEnv {
        fn close_flag(&self) -> bool {
            false
        }

        fn close_status(&self) -> Status {
            tdutils::status::Status::ok()
        }

        fn auth_is_bot(&self) -> bool {
            false
        }

        fn now(&self) -> f64 {
            1000.0
        }

        fn unix_time(&self) -> i32 {
            1000
        }

        fn use_chat_info_database(&self) -> bool {
            false
        }

        fn my_user_id(&self) -> UserId {
            UserId::new(1)
        }

        fn my_dialog_id(&self) -> DialogId {
            DialogId::from_user_id(UserId::new(1))
        }

        fn have_dialog_force(&mut self, dialog_id: DialogId, _source: &str) -> bool {
            match dialog_id.get_type() {
                DialogType::User => self.users.contains(&dialog_id.get_user_id().get()),
                DialogType::Channel => true,
                _ => false,
            }
        }

        fn have_dialog_info(&mut self, dialog_id: DialogId) -> bool {
            self.have_dialog_force(dialog_id, "have_dialog_info")
        }

        fn check_dialog_access_read(
            &mut self,
            dialog_id: DialogId,
            _source: &str,
        ) -> Result<(), Status> {
            if self.have_dialog_force(dialog_id, "check_dialog_access") {
                Ok(())
            } else {
                Err(Status::error(400, "Chat not found"))
            }
        }

        fn check_dialog_access_write(
            &mut self,
            dialog_id: DialogId,
            source: &str,
        ) -> Result<(), Status> {
            self.check_dialog_access_read(dialog_id, source)
        }

        fn is_monoforum_channel(&mut self, _dialog_id: DialogId) -> bool {
            false
        }

        fn get_dialog_status(&mut self, _dialog_id: DialogId) -> DialogParticipantStatus {
            DialogParticipantStatus::member(0, String::new())
        }

        fn is_dialog_opened(&self, _dialog_id: DialogId) -> bool {
            false
        }

        fn get_dialog_search_text(&self, _dialog_id: DialogId) -> String {
            String::new()
        }

        fn get_chat_id_object(&mut self, dialog_id: DialogId, _source: &str) -> i64 {
            dialog_id.get()
        }

        fn get_input_peer_know(&mut self, dialog_id: DialogId) -> Option<telegram_api::InputPeer> {
            if self.have_dialog_force(dialog_id, "get_input_peer") {
                self.get_participant_peers.push(dialog_id);
                Some(telegram_api::InputPeer::inputPeerUser(
                    telegram_api::inputPeerUser {
                        user_id: dialog_id.get_user_id().get(),
                        access_hash: 1,
                    },
                ))
            } else {
                None
            }
        }

        fn on_get_dialog_error(
            &mut self,
            _dialog_id: DialogId,
            _status: &Status,
            _source: &str,
        ) -> bool {
            false
        }

        fn get_chat_is_active(&mut self, _chat_id: ChatId) -> bool {
            false
        }

        fn get_chat_status(&mut self, _chat_id: ChatId) -> DialogParticipantStatus {
            DialogParticipantStatus::member(0, String::new())
        }

        fn get_channel_status(&mut self, _channel_id: ChannelId) -> DialogParticipantStatus {
            DialogParticipantStatus::member(0, String::new())
        }

        fn get_channel_permissions(&mut self, _channel_id: ChannelId) -> DialogParticipantStatus {
            DialogParticipantStatus::member(0, String::new())
        }

        fn is_broadcast_channel(&mut self, _channel_id: ChannelId) -> bool {
            false
        }

        fn is_megagroup_channel(&mut self, _channel_id: ChannelId) -> bool {
            true
        }

        fn get_channel_type(&mut self, _channel_id: ChannelId) -> ChannelType {
            ChannelType::Megagroup
        }

        fn get_channel_effective_has_hidden_participants(
            &mut self,
            _channel_id: ChannelId,
            _source: &str,
        ) -> bool {
            false
        }

        fn get_input_channel(
            &mut self,
            _channel_id: ChannelId,
        ) -> Option<telegram_api::InputChannel> {
            Some(telegram_api::InputChannel::inputChannel(
                telegram_api::inputChannel {
                    channel_id: 1,
                    access_hash: 1,
                },
            ))
        }

        fn invalidate_channel_full(
            &mut self,
            _channel_id: ChannelId,
            _need_drop_slow_mode_delay: bool,
            _source: &str,
        ) {
        }

        fn on_get_chats(&mut self, _chats: Vec<telegram_api::Chat>, _source: &str) {}

        fn on_get_channel_error(&mut self, _channel_id: ChannelId, status: &Status, _source: &str) {
            self.channel_errors.push(status.clone());
        }

        fn get_chat_member_object(
            &mut self,
            participant: &DialogParticipant,
            _source: &str,
        ) -> td_api::ChatMember {
            td_api::ChatMember {
                member_id: Some(td_api::MessageSender::messageSenderUser(
                    td_api::messageSenderUser {
                        user_id: participant.dialog_id_.get(),
                    },
                )),
                tag: String::new(),
                inviter_user_id: 0,
                joined_chat_date: 0,
                status: None,
            }
        }

        fn get_chat_participants(&mut self, _chat_id: ChatId) -> Option<Vec<DialogParticipant>> {
            None
        }

        fn load_chat_full(&mut self, _chat_id: ChatId) {}

        fn on_update_channel_bot_user_ids(
            &mut self,
            _channel_id: ChannelId,
            _bot_user_ids: Vec<UserId>,
        ) {
        }

        fn on_update_channel_participant_count(&mut self, _channel_id: ChannelId, _count: i32) {}

        fn on_update_channel_administrator_count(&mut self, _channel_id: ChannelId, _count: i32) {}

        fn have_user_force(&mut self, user_id: UserId, _source: &str) -> bool {
            self.users.contains(&user_id.get())
        }

        fn is_user_deleted(&mut self, _user_id: UserId) -> bool {
            false
        }

        fn have_min_user(&mut self, user_id: UserId) -> bool {
            self.users.contains(&user_id.get())
        }

        fn get_user_was_online(&mut self, _user_id: UserId, _unix_time: i32) -> i32 {
            0
        }

        fn is_user_online(&mut self, _user_id: UserId, _unix_time: i32) -> bool {
            false
        }

        fn get_input_user(&mut self, _user_id: UserId) -> Result<telegram_api::InputUser, Status> {
            Ok(telegram_api::InputUser::inputUser(
                telegram_api::inputUser {
                    user_id: 1,
                    access_hash: 1,
                },
            ))
        }

        fn on_get_users(&mut self, _users: Vec<telegram_api::User>, _source: &str) {}

        fn get_secret_chat_user_id(&mut self, _dialog_id: DialogId) -> UserId {
            UserId::new(0)
        }

        fn chat_administrator_object(
            &mut self,
            administrator: &DialogAdministrator,
        ) -> td_api::ChatAdministrator {
            td_api::ChatAdministrator {
                user_id: administrator.get_user_id().get(),
                custom_title: administrator.get_rank().to_owned(),
                is_owner: administrator.is_creator(),
                can_be_edited: administrator.can_be_edited(),
            }
        }

        fn status_update_restrictions(&mut self, _status: &mut DialogParticipantStatus) {}

        fn have_dialog(&mut self, dialog_id: DialogId) -> bool {
            self.have_dialog_force(dialog_id, "have_dialog")
        }

        fn on_view_dialog_active_stories(&mut self, _dialog_ids: Vec<DialogId>) {}

        fn chat_members_object(
            &mut self,
            total_count: i32,
            participants: Vec<DialogParticipant>,
            _source: &str,
        ) -> td_api::ChatMembers {
            td_api::ChatMembers {
                total_count,
                members: participants
                    .iter()
                    .map(|participant| self.get_chat_member_object(participant, "test"))
                    .collect(),
            }
        }

        fn send_null_user(&mut self, _promise: Promise<td_api::User>) {}

        fn sqlite_pmc_get_raw(&mut self, _key: &[u8]) -> Vec<u8> {
            Vec::new()
        }

        fn sqlite_pmc_set_raw(&mut self, _key: &[u8], _value: &[u8]) {}

        fn sqlite_pmc_erase(&mut self, _key: &[u8]) {}

        fn set_online_member_count_timeout(&mut self, _dialog_id: DialogId, _timeout: f64) {}

        fn set_channel_participant_cache_timeout(&mut self, _channel_id: ChannelId, _timeout: f64) {
        }

        fn send_update_chat_online_member_count(
            &mut self,
            _dialog_id: DialogId,
            _online_member_count: i32,
        ) {
        }

        fn get_channel_administrators_query(&mut self, _channel_id: ChannelId, _hash: i64) {}

        fn send_get_channel_participants_query(
            &mut self,
            channel_id: ChannelId,
            _filter: &ChannelParticipantFilter,
            offset: i32,
            limit: i32,
        ) -> Result<(), Status> {
            if let Some(error) = if self.send_errors.is_empty() {
                None
            } else {
                Some(self.send_errors.remove(0))
            } {
                return Err(error);
            }
            self.sent.push("getParticipants");
            self.get_participants_queries
                .push((channel_id, offset, limit));
            Ok(())
        }

        fn send_get_channel_participant_query(
            &mut self,
            _channel_id: ChannelId,
            _input_peer: telegram_api::InputPeer,
        ) -> Result<(), Status> {
            if let Some(error) = if self.send_errors.is_empty() {
                None
            } else {
                Some(self.send_errors.remove(0))
            } {
                return Err(error);
            }
            self.sent.push("getParticipant");
            Ok(())
        }

        fn send_edit_chat_creator_query(
            &mut self,
            dialog_id: DialogId,
            user_id: UserId,
            _input_check_password: telegram_api::InputCheckPasswordSRP,
        ) -> Result<(), Status> {
            if let Some(error) = if self.send_errors.is_empty() {
                None
            } else {
                Some(self.send_errors.remove(0))
            } {
                return Err(error);
            }
            self.sent.push("editChatCreator");
            self.edit_creator_queries.push((dialog_id, user_id));
            Ok(())
        }

        fn get_chat_join_requests_query(
            &mut self,
            _dialog_id: DialogId,
            _invite_link: String,
            _query: String,
            _offset_date: i32,
            _offset_user_id: UserId,
            _limit: i32,
        ) {
        }

        fn hide_chat_join_request_query(
            &mut self,
            _dialog_id: DialogId,
            _user_id: UserId,
            _approve: bool,
        ) {
        }

        fn hide_all_chat_join_requests_query(
            &mut self,
            _dialog_id: DialogId,
            _invite_link: String,
            _approve: bool,
        ) {
        }

        fn can_edit_chat_creator_query(&mut self) {}

        fn get_future_chat_creator_query(&mut self, _dialog_id: DialogId) {}

        fn transfer_chat_ownership_seam(
            &mut self,
            _dialog_id: DialogId,
            _user_id: UserId,
            _password: String,
        ) {
            // the SRP half is blocked here too — the lot entry stays parked
        }

        fn edit_chat_participant_rank_query(
            &mut self,
            _dialog_id: DialogId,
            _user_id: UserId,
            _rank: String,
        ) {
        }

        fn request_chat_join_web_view_query(&mut self, _query_id: i64) {}

        fn set_join_chat_results_query(
            &mut self,
            _query_id: i64,
            _result: td_api::ChatJoinRequestResult,
            _url: String,
        ) {
        }

        fn join_or_add_chat_member_query(&mut self, _chat_id: ChatId, _user_id: UserId) {}

        fn join_channel_query(&mut self, _channel_id: ChannelId) {}

        fn invite_to_channel_query(&mut self, _channel_id: ChannelId, _user_id: UserId) {}

        fn edit_channel_participant_query(
            &mut self,
            _channel_id: ChannelId,
            _participant_dialog_id: DialogId,
            _new_status: DialogParticipantStatus,
        ) {
        }

        fn edit_chat_participant_query(
            &mut self,
            _chat_id: ChatId,
            _user_id: UserId,
            _new_status: DialogParticipantStatus,
        ) {
        }
    }

    #[test]
    fn get_channel_participant_parks_and_settles_in_fifo_order() {
        let mut env = BandEnv::new(&[900]);
        let mut manager = DialogParticipantManager::new();
        let channel_id = ChannelId::new(705);

        // two parks — the promises land in the family lot in order
        let (first, first_outcomes) = {
            let sink = Arc::new(std::sync::Mutex::new(Vec::new()));
            (
                Promise::from_interface(RecMember {
                    settled: Arc::clone(&sink),
                }),
                sink,
            )
        };
        let (second, second_outcomes) = {
            let sink = Arc::new(std::sync::Mutex::new(Vec::new()));
            (
                Promise::from_interface(RecMember {
                    settled: Arc::clone(&sink),
                }),
                sink,
            )
        };
        manager.get_channel_participant(
            &mut env,
            channel_id,
            DialogId::from_user_id(UserId::new(900)),
            first,
        );
        manager.get_channel_participant(
            &mut env,
            channel_id,
            DialogId::from_user_id(UserId::new(900)),
            second,
        );
        assert_eq!(env.sent, vec!["getParticipant", "getParticipant"]);
        assert_eq!(manager.pending.channel_participant.len(), 2);

        // the answers pair FIFO: the first answer settles the FIRST promise
        manager.on_get_channel_participant_answer(
            &mut env,
            Ok(telegram_api::channels_ChannelParticipant {
                participant: Some(telegram_api::ChannelParticipant::channelParticipant(
                    telegram_api::channelParticipant {
                        flags: 0,
                        user_id: 900,
                        date: 1,
                        subscription_until_date: 0,
                        rank: String::new(),
                    },
                )),
                chats: vec![],
                users: vec![],
            }),
        );
        assert_eq!(first_outcomes.lock().unwrap().len(), 1);
        assert!(second_outcomes.lock().unwrap().is_empty());
        manager.on_get_channel_participant_answer(
            &mut env,
            Err(Status::error(400, "USER_NOT_PARTICIPANT")),
        );
        assert_eq!(second_outcomes.lock().unwrap().len(), 1);
        assert!(manager.pending.channel_participant.is_empty());
    }

    #[test]
    fn get_channel_participant_settles_the_send_leg_failure() {
        let mut env = BandEnv::new(&[900]);
        env.fail_next_send_with(Status::error(400, "Supergroup not found"));
        let mut manager = DialogParticipantManager::new();

        let (promise, outcomes) = {
            let sink = Arc::new(std::sync::Mutex::new(Vec::new()));
            (
                Promise::from_interface(RecMember {
                    settled: Arc::clone(&sink),
                }),
                sink,
            )
        };
        manager.get_channel_participant(
            &mut env,
            ChannelId::new(705),
            DialogId::from_user_id(UserId::new(900)),
            promise,
        );
        // the C++ band settles the handler promise before any wire query
        assert!(env.sent.is_empty());
        assert!(manager.pending.channel_participant.is_empty());
        assert!(
            matches!(outcomes.lock().unwrap().first(), Some(Err(error)) if error.message() == "Supergroup not found")
        );
    }

    #[test]
    fn get_channel_participants_gates_and_answer_tail() {
        let mut env = BandEnv::new(&[900]);
        let mut manager = DialogParticipantManager::new();
        let channel_id = ChannelId::new(705);
        let filter = ChannelParticipantFilter::recent();

        // the local gates answer before any park
        let (promise, outcomes) = {
            let sink = Arc::new(std::sync::Mutex::new(Vec::new()));
            (
                Promise::from_interface(RecMembers {
                    settled: Arc::clone(&sink),
                }),
                sink,
            )
        };
        manager.get_channel_participants(
            &mut env,
            channel_id,
            &filter,
            String::new(),
            0,
            0,
            -1,
            promise,
            "getSupergroupMembers",
        );
        assert!(
            matches!(&outcomes.lock().unwrap()[0], Err(message) if message.message() == "Parameter limit must be positive")
        );
        let (promise, outcomes) = {
            let sink = Arc::new(std::sync::Mutex::new(Vec::new()));
            (
                Promise::from_interface(RecMembers {
                    settled: Arc::clone(&sink),
                }),
                sink,
            )
        };
        manager.get_channel_participants(
            &mut env,
            channel_id,
            &filter,
            String::new(),
            -1,
            10,
            -1,
            promise,
            "getSupergroupMembers",
        );
        assert!(
            matches!(&outcomes.lock().unwrap()[0], Err(message) if message.message() == "Parameter offset must be non-negative")
        );

        // the MAX clamp rides the wire query
        let (promise, _outcomes) = {
            let sink = Arc::new(std::sync::Mutex::new(Vec::new()));
            (
                Promise::from_interface(RecMembers {
                    settled: Arc::clone(&sink),
                }),
                sink,
            )
        };
        manager.get_channel_participants(
            &mut env,
            channel_id,
            &filter,
            String::new(),
            0,
            500,
            -1,
            promise,
            "getSupergroupMembers",
        );
        assert_eq!(
            env.get_participants_queries
                .last()
                .map(|(_, _, limit)| *limit),
            Some(MAX_GET_CHANNEL_PARTICIPANTS)
        );
        assert_eq!(manager.pending.channel_participants.len(), 1);

        // drain the clamped page (an error settles the parked head)
        manager
            .on_get_channel_participants_answer(&mut env, Err(Status::error(420, "FLOOD_WAIT_9")));
        assert!(manager.pending.channel_participants.is_empty());

        // the answer tail: the unsuitable member drops and the total-count
        // fixups run (the bots filter against a human member)
        let mut env = BandEnv::new(&[900]);
        env.users.clear(); // nobody is a bot in this env
        let bots_filter = ChannelParticipantFilter {
            filter_type: crate::dialog_participant_filter::ChannelParticipantFilterType::Bots,
            query: String::new(),
            message_topic: None,
        };
        manager.get_channel_participants(
            &mut env,
            channel_id,
            &bots_filter,
            String::new(),
            0,
            50,
            -1,
            Promise::from_interface(RecMembers::default()),
            "getSupergroupMembers",
        );
        manager.on_get_channel_participants_answer(
            &mut env,
            Ok(telegram_api::channels_channelParticipants {
                count: 1,
                participants: vec![telegram_api::ChannelParticipant::channelParticipant(
                    telegram_api::channelParticipant {
                        flags: 0,
                        user_id: 900,
                        date: 1,
                        subscription_until_date: 0,
                        rank: String::new(),
                    },
                )],
                chats: vec![],
                users: vec![],
            }),
        );
        assert!(manager.pending.channel_participants.is_empty());
    }

    #[test]
    fn transfer_parks_at_the_srp_seam_and_the_tail_sends() {
        let mut env = BandEnv::new(&[900]);
        let mut manager = DialogParticipantManager::new();
        let channel_dialog = DialogId::from_channel_id(ChannelId::new(705));

        // the request arm parks at the [blocked: PasswordManager] seam —
        // its own lot, nothing sends
        manager.transfer_dialog_ownership(
            &mut env,
            channel_dialog,
            UserId::new(900),
            "pw",
            Promise::from_interface(RecUnit::default()),
        );
        assert!(env.sent.is_empty());
        assert_eq!(manager.pending.transfer_ownership_srp.len(), 1);
        assert!(manager.pending.edit_chat_creator.is_empty());

        // the resolved-SRP tail sends and the answer drains
        let (promise, outcomes) = {
            let sink = Arc::new(std::sync::Mutex::new(Vec::new()));
            (
                Promise::from_interface(RecUnit {
                    settled: Arc::clone(&sink),
                }),
                sink,
            )
        };
        env.fail_next_send_with(Status::error(400, "Supergroup not found"));
        let (failing, failing_outcomes) = {
            let sink = Arc::new(std::sync::Mutex::new(Vec::new()));
            (
                Promise::from_interface(RecUnit {
                    settled: Arc::clone(&sink),
                }),
                sink,
            )
        };
        manager.transfer_chat_ownership(
            &mut env,
            channel_dialog,
            UserId::new(900),
            telegram_api::InputCheckPasswordSRP::inputCheckPasswordEmpty(
                telegram_api::inputCheckPasswordEmpty {},
            ),
            failing,
        );
        assert!(manager.pending.edit_chat_creator.is_empty());
        assert!(matches!(
            failing_outcomes.lock().unwrap().first(),
            Some(Err(_))
        ));

        manager.transfer_chat_ownership(
            &mut env,
            channel_dialog,
            UserId::new(900),
            telegram_api::InputCheckPasswordSRP::inputCheckPasswordEmpty(
                telegram_api::inputCheckPasswordEmpty {},
            ),
            promise,
        );
        assert_eq!(env.sent, vec!["editChatCreator"]);
        assert_eq!(manager.pending.edit_chat_creator.len(), 1);
        // the answer returns the updates walk the promise rides (the drain
        // feeds it to the updates manager, which settles it)
        let (_, promise) = manager
            .on_edit_chat_creator_answer(
                &mut env,
                Ok(telegram_api::Updates::updatesTooLong(
                    telegram_api::updatesTooLong {},
                )),
            )
            .expect("the updates walk");
        let mut promise = promise;
        promise.set_value(());
        assert!(manager.pending.edit_chat_creator.is_empty());
        assert!(matches!(outcomes.lock().unwrap().first(), Some(Ok(()))));
    }
}
