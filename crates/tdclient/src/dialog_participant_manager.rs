//! `td::DialogParticipantManager` (DialogParticipantManager.{h,cpp}) — the
//! membership plane upstream extracted from MessagesManager: administrators,
//! member queries/search, join/leave/add/ban mutations, join requests and the
//! ownership-transfer machine.
//!
//! Slice 1 (the M.10 next-slices queue): every request arm's LOCAL decision
//! plane is C++-exact; the wire query sends are the documented
//! `[blocked: net]` env seams whose promises park exactly like the C++
//! request without a network.

use crate::chat_manager::{
    AdministratorRights, ChannelId, ChannelType, ChatId, DialogAdministrator, DialogParticipant,
    DialogParticipantStatus, RestrictedRights,
};
use crate::chat_manager::DialogType;
use crate::chat_manager::DialogId;
use crate::dialog_participant_filter::{ChannelParticipantFilter, DialogParticipantFilter};
use crate::misc::get_vector_hash;
use crate::store_parse::{store_i32, Parser};
use crate::td_api;
use crate::user_manager::UserId;
use std::collections::HashMap;
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

/// the vector<DialogAdministrator> log-event codec — the C++
/// `log_event_store(vector)` shape: an i32 size, then each element's store.
pub fn store_dialog_administrators(administrators: &[DialogAdministrator], out: &mut Vec<u8>) {
    store_i32(out, administrators.len() as i32);
    for administrator in administrators {
        administrator.store(out);
    }
}

/// the inverse of [`store_dialog_administrators`].
pub fn parse_dialog_administrators(parser: &mut Parser<'_>) -> Result<Vec<DialogAdministrator>, &'static str> {
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
    fn check_dialog_access_read(&mut self, dialog_id: DialogId, source: &str) -> Result<(), Status>;
    fn check_dialog_access_write(&mut self, dialog_id: DialogId, source: &str) -> Result<(), Status>;
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
    /// `dialog_manager_->get_input_peer(dialog_id, AccessRights::Know) !=
    /// nullptr`.
    fn have_input_peer_know(&mut self, dialog_id: DialogId) -> bool;

    // -- ChatManager probes
    fn get_chat_is_active(&mut self, chat_id: ChatId) -> bool;
    fn get_chat_status(&mut self, chat_id: ChatId) -> DialogParticipantStatus;
    fn get_channel_status(&mut self, channel_id: ChannelId) -> DialogParticipantStatus;
    /// `chat_manager_->get_channel_permissions(channel_id)`.
    fn get_channel_permissions(&mut self, channel_id: ChannelId) -> DialogParticipantStatus;
    fn is_broadcast_channel(&mut self, channel_id: ChannelId) -> bool;
    /// `chat_manager_->get_chat_member_object(participant, source)`.
    fn get_chat_member_object(&mut self, participant: &DialogParticipant, source: &str) -> td_api::ChatMember;
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
    /// `user_manager_->get_secret_chat_user_id(secret_chat_id)`.
    fn get_secret_chat_user_id(&mut self, dialog_id: DialogId) -> UserId;
    /// `DialogAdministrator::get_chat_administrator_object(user_manager)`.
    fn chat_administrator_object(&mut self, administrator: &DialogAdministrator) -> td_api::ChatAdministrator;
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
    fn send_update_chat_online_member_count(&mut self, dialog_id: DialogId, online_member_count: i32);

    // -- the [blocked: net] query seams. Each parks its promise — the
    // -- answers would come from the query callbacks, which never fire
    // -- offline; the C++ request without a network behaves identically.
    /// `GetChannelAdministratorsQuery::send(channel_id, hash)`; the result
    /// re-enters `on_reload_dialog_administrators`.
    fn get_channel_administrators_query(&mut self, channel_id: ChannelId, hash: i64);
    /// `GetChannelParticipantsQuery::send(channel_id, filter, offset, limit)`;
    /// the result re-enters `on_get_channel_participants` [blocked: net].
    fn get_channel_participants_query(
        &mut self,
        channel_id: ChannelId,
        filter: &ChannelParticipantFilter,
        offset: i32,
        limit: i32,
    );
    /// `GetChannelParticipantQuery::send(channel_id, participant_dialog_id,
    /// input_peer)`; the result re-enters `finish_get_channel_participant`.
    fn get_channel_participant_query(&mut self, channel_id: ChannelId, participant_dialog_id: DialogId);
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
    fn hide_all_chat_join_requests_query(&mut self, dialog_id: DialogId, invite_link: String, approve: bool);
    /// `CanEditChatCreatorQuery::send()` — the STATUS error comes back into
    /// [`DialogParticipantManager::map_can_transfer_ownership_error`];
    /// offline it parks.
    fn can_edit_chat_creator_query(&mut self);
    /// `GetFutureChatCreatorAfterLeaveQuery::send(dialog_id)`.
    fn get_future_chat_creator_query(&mut self, dialog_id: DialogId);
    /// `PasswordManager::get_input_check_password_srp(password, ...)` then
    /// `EditChatCreatorQuery::send` — both halves [blocked: net/Secret].
    fn transfer_chat_ownership_seam(&mut self, dialog_id: DialogId, user_id: UserId);
    /// `EditChatParticipantRankQuery::send(dialog_id, user_id, rank)`.
    fn edit_chat_participant_rank_query(&mut self, dialog_id: DialogId, user_id: UserId, rank: String);
    /// `RequestChatJoinWebViewQuery::send(query_id, parameters)`.
    fn request_chat_join_web_view_query(&mut self, query_id: i64);
    /// `SetJoinChatResultsQuery::send(query_id, result, url)`.
    fn set_join_chat_results_query(&mut self, query_id: i64, result: td_api::ChatJoinRequestResult, url: String);
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
            self.set_dialog_online_member_count(env, dialog_id, online_member_count, is_from_server);
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
        let need_update =
            !info.is_update_sent || now > info.update_time + ONLINE_MEMBER_COUNT_CACHE_EXPIRE_TIME as f64;
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
        let Some(info) = self.dialog_online_member_counts.get(&dialog_id.get()).copied() else {
            return;
        };
        if env.now() > info.update_time + ONLINE_MEMBER_COUNT_CACHE_EXPIRE_TIME as f64 {
            self.online_member_count_deadlines.remove(&dialog_id.get());
            env.set_online_member_count_timeout(dialog_id, 0.0);
            self.dialog_online_member_counts.remove(&dialog_id.get());
        } else if info.is_update_sent {
            self.set_dialog_online_member_count(env, dialog_id, info.online_member_count, is_from_server);
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
                return promise.take().unwrap().set_value(td_api::ChatAdministrators {
                    administrators: Vec::new(),
                });
            }
            DialogType::Chat => {}
            DialogType::Channel => {
                if env.is_monoforum_channel(dialog_id) {
                    return promise.take().unwrap().set_value(td_api::ChatAdministrators {
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
            let value = env.sqlite_pmc_get_raw(&Self::get_dialog_administrators_database_key(dialog_id));
            return self.on_load_dialog_administrators_from_database(env, dialog_id, value, promise.take().unwrap());
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
        if !administrators.iter().all(|administrator| env.have_min_user(administrator.get_user_id())) {
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
                env.sqlite_pmc_set_raw(&Self::get_dialog_administrators_database_key(dialog_id), &out);
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
                        self.on_update_dialog_administrators(env, dialog_id, administrators.clone(), true, false);
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
                    self.on_update_dialog_administrators(env, dialog_id, administrators, true, false);
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
                self.get_channel_participant(env, dialog_id.get_channel_id(), participant_dialog_id, promise);
            }
            DialogType::SecretChat => {
                let my_user_id = env.my_user_id();
                let peer_user_id = env.get_secret_chat_user_id(dialog_id);
                let participant = if participant_dialog_id == DialogId::from_user_id(my_user_id) {
                    DialogParticipant::private_member(my_user_id, peer_user_id)
                } else if peer_user_id.is_valid() && participant_dialog_id == DialogId::from_user_id(peer_user_id) {
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
        let participant_dialog_id = dialog_participant.dialog_id_;
        let is_user = participant_dialog_id.get_type() == DialogType::User;
        if (is_user && !env.have_min_user(participant_dialog_id.get_user_id()))
            || (!is_user && !env.have_dialog(participant_dialog_id))
        {
            return promise.set_error(Status::error(400, "Member not found"));
        }
        let object = env.get_chat_member_object(&dialog_participant, "finish_get_dialog_participant");
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
        if !env.have_input_peer_know(participant_dialog_id) {
            return promise.set_error(Status::error(400, "Member not found"));
        }
        if env.is_broadcast_channel(channel_id) && !env.get_channel_status(channel_id).is_administrator() {
            return promise.set_error(Status::error(400, "Member list is inaccessible"));
        }

        if self.have_channel_participant_cache(channel_id) {
            if let Some(participant) = self.get_channel_participant_from_cache(channel_id, participant_dialog_id) {
                let participant = participant.clone();
                return self.finish_get_dialog_participant(env, participant, promise);
            }
        }

        // `GetChannelParticipantQuery` [blocked: net] — the promise parks
        env.get_channel_participant_query(channel_id, participant_dialog_id);
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
                    if user_id == my_user_id { peer_user_id } else { my_user_id },
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
    /// `GetChannelParticipantsQuery` seam; the response machine
    /// (`on_get_channel_participants`) is the [blocked: net] intake.
    pub fn get_channel_participants(
        &mut self,
        env: &mut impl DialogParticipantManagerEnv,
        channel_id: ChannelId,
        filter: &ChannelParticipantFilter,
        offset: i32,
        limit: i32,
        mut promise: Promise<td_api::ChatMembers>,
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

        // `GetChannelParticipantsQuery` [blocked: net] — the promise parks
        env.get_channel_participants_query(channel_id, filter, offset, limit);
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
                let participants =
                    self.search_private_chat_participants(env, dialog_id.get_user_id(), query, limit, filter);
                promise.set_value(env.chat_members_object(
                    participants.total_count,
                    participants.participants,
                    "searchChatMembers",
                ));
            }
            DialogType::Chat => {
                self.search_chat_participants(env, dialog_id.get_chat_id(), query, limit, filter, promise);
            }
            DialogType::Channel => {
                let channel_id = dialog_id.get_channel_id();
                if filter.has_query() {
                    self.get_channel_participants(
                        env,
                        channel_id,
                        &filter.as_channel_participant_filter(""),
                        0,
                        limit,
                        promise,
                    );
                } else {
                    // the C++ carries the user query as the additional search
                    // over a fixed 100-member first page
                    self.get_channel_participants(
                        env,
                        channel_id,
                        &filter.as_channel_participant_filter(query),
                        0,
                        100,
                        promise,
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
            DialogType::SecretChat => promise.set_error(Status::error(400, "Can't join secret chats")),
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
            DialogType::User => promise.set_error(Status::error(400, "Can't add members to a private chat")),
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
            DialogType::User => promise.set_error(Status::error(400, "Can't add members to a private chat")),
            DialogType::Chat => {
                if user_ids.len() == 1 {
                    env.join_or_add_chat_member_query(dialog_id.get_chat_id(), user_ids[0]);
                    return;
                }
                promise.set_error(Status::error(400, "Can't add many members at once to a basic group chat"))
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
            DialogType::User => {
                promise.set_error(Status::error(400, "Chat member status can't be changed in private chats"))
            }
            DialogType::Chat => {
                let status = get_dialog_participant_status(env, Some(&chat_member_status), ChannelType::Unknown);
                if participant_dialog_id.get_type() != DialogType::User {
                    if status == DialogParticipantStatus::left() {
                        return promise.set_value(());
                    }
                    return promise.set_error(Status::error(400, "Chats can't be members of basic groups"));
                }
                env.edit_chat_participant_query(
                    dialog_id.get_chat_id(),
                    participant_dialog_id.get_user_id(),
                    status,
                );
            }
            DialogType::Channel => {
                let status = get_dialog_participant_status(env, Some(&chat_member_status), ChannelType::Unknown);
                env.edit_channel_participant_query(dialog_id.get_channel_id(), participant_dialog_id, status);
            }
            DialogType::SecretChat => {
                promise.set_error(Status::error(400, "Chat member status can't be changed in secret chats"))
            }
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
        if let Err(error) = env.check_dialog_access_read(dialog_id, "set_dialog_participant_rank 1") {
            return promise.set_error(error);
        }
        match dialog_id.get_type() {
            DialogType::User => {
                promise.set_error(Status::error(400, "Chat member tag can't be changed in private chats"))
            }
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
            DialogType::User => promise.set_error(Status::error(400, "Can't ban members in private chats")),
            DialogType::Chat => {
                if participant_dialog_id.get_type() != DialogType::User {
                    return promise.set_error(Status::error(400, "Can't ban chats in basic groups"));
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
            DialogType::SecretChat => promise.set_error(Status::error(400, "Can't ban members in secret chats")),
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
            DialogType::SecretChat => promise.set_error(Status::error(400, "Can't leave secret chats")),
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
                    return Err(Status::error(400, "Not enough rights to manage chat join requests"));
                }
                Ok(())
            }
            DialogType::Channel => {
                if !env
                    .get_channel_status(dialog_id.get_channel_id())
                    .can_manage_invite_links()
                {
                    return Err(Status::error(400, "Not enough rights to manage chat join requests"));
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
                DialogType::Chat => {
                    !env.get_chat_status(dialog_id.get_chat_id()).can_manage_invite_links()
                }
                DialogType::Channel => {
                    !env
                        .get_channel_permissions(dialog_id.get_channel_id())
                        .can_manage_invite_links()
                }
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

        env.get_chat_join_requests_query(dialog_id, invite_link, query, offset_date, offset_user_id, limit);
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
    pub fn map_can_transfer_ownership_error(error: &Status) -> Result<CanTransferOwnershipResult, Status> {
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
    /// (DialogParticipantManager.cpp:3352-3390) — the local gates; the
    /// SRP + `EditChatCreatorQuery` halves are the seam.
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
                env.transfer_chat_ownership_seam(dialog_id, user_id);
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
        if let Err(error) = env.check_dialog_access_write(dialog_id, "get_future_creator_after_leave") {
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
        let entry = self.channel_participants.entry(channel_id.get()).or_default();
        match entry.get_mut(&dialog_participant.dialog_id_.get()) {
            Some(existing) => {
                if !allow_replace {
                    return;
                }
                *existing = (dialog_participant, unix_time);
            }
            None => {
                entry.insert(dialog_participant.dialog_id_.get(), (dialog_participant, unix_time));
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
                let dialog_participant =
                    DialogParticipant::new(participant_dialog_id, UserId::new(0), unix_time, dialog_participant_status);
                entry.insert(participant_dialog_id.get(), (dialog_participant, unix_time));
            }
        }
    }

    /// `DialogParticipantManager::drop_channel_participant_cache`
    /// (DialogParticipantManager.cpp:3190-3192).
    pub fn drop_channel_participant_cache(&mut self, channel_id: ChannelId) {
        self.channel_participants.remove(&channel_id.get());
        self.channel_participant_cache_deadlines.remove(&channel_id.get());
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

    #[test]
    fn can_transfer_ownership_error_mapping() {
        let result = DialogParticipantManager::map_can_transfer_ownership_error(&Status::error(400, "PASSWORD_HASH_INVALID"))
            .unwrap();
        assert_eq!(result.result_type, CanTransferOwnershipResultType::Ok);

        let result = DialogParticipantManager::map_can_transfer_ownership_error(&Status::error(400, "PASSWORD_MISSING"))
            .unwrap();
        assert_eq!(result.result_type, CanTransferOwnershipResultType::PasswordNeeded);
        assert_eq!(result.retry_after, 0);

        let result = DialogParticipantManager::map_can_transfer_ownership_error(&Status::error(400, "PASSWORD_TOO_FRESH_1234"))
            .unwrap();
        assert_eq!(result.result_type, CanTransferOwnershipResultType::PasswordTooFresh);
        assert_eq!(result.retry_after, 1234);

        let result = DialogParticipantManager::map_can_transfer_ownership_error(&Status::error(400, "SESSION_TOO_FRESH_-7"))
            .unwrap();
        assert_eq!(result.result_type, CanTransferOwnershipResultType::SessionTooFresh);
        // the C++ clamps a negative retry_after to 0
        assert_eq!(result.retry_after, 0);

        assert!(DialogParticipantManager::map_can_transfer_ownership_error(&Status::error(429, "TELEGRAM_SERVER_ERROR")).is_err());
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
        let td_api::CanTransferOwnershipResult::canTransferOwnershipResultPasswordTooFresh(inner) = object
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
}
