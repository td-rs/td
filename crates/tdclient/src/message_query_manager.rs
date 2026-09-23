//! Port of `td/telegram/MessageQueryManager.{h,cpp}` (M.18) — the planes
//! that need no network and no message plane, sliced per the Wave-2 plan:
//! the value/record planes (`MessageThreadInfo.h`, `MessageViewer.{h,cpp}`,
//! `AffectedHistory.h`, `EmojiGameInfo.{h,cpp}`, `SearchPostsFlood.{h,cpp}`,
//! `MessageSearchOffset.{h,cpp}`), the byte-exact binlog logevent
//! store/parse family (MessageQueryManager.cpp:4079-5142) and, in the
//! follow-up slice, the manager's local machinery (the pending-read
//! counters, the reactions-reload queue, the view-metrics buffer, the
//! affected-history loop and the emoji-game cache).
//!
//! Heavily M.10-coupled surfaces (threads/forwards/statistics over
//! MessagesManager) carry the `[blocked: M.10 integration]` seam tag; the
//! network query tails carry `[blocked: net]` — the collapsed-actor
//! convention: `Td` owns the struct, the [`MessageQueryManagerEnv`] trait
//! carries the `G()`/option/cross-manager surfaces and the promise lands
//! unanswered offline.

use crate::chat_manager::ChannelId;
use crate::dialog_id::{DialogId, MessageFullId, ServerMessageId};
use crate::messages_manager::{EphemeralMessageId, MessageId};
use crate::store_parse::{self, Parser};
use crate::td_api;
use crate::user_manager::UserId;
use std::collections::{HashMap, HashSet};
use tdmtproto::telegram_api;
use tdutils::promise::Promise;

/// `MessageQueryManager::MAX_SEARCH_MESSAGES` (MessageQueryManager.h:351) —
/// the server-side search limit the request arms clamp to; its consumers
/// (the search arms) land with the M.18.4 wiring.
#[allow(dead_code)]
pub(crate) const MAX_SEARCH_MESSAGES: i32 = 100;

/// The `G()`/manager slice the collapsed MessageQueryManager reads through.
pub trait MessageQueryManagerEnv {
    // -- the always-required surfaces
    /// `td_->auth_manager_->is_bot()`.
    fn auth_is_bot(&self) -> bool;
    /// `G()->close_flag()`.
    fn close_flag(&self) -> bool;
    /// `G()->unix_time()`.
    fn unix_time(&self) -> i32;
    /// `Time::now()` — the process-monotonic clock.
    fn now(&self) -> f64;
    /// `td_->option_manager_->get_option_string(name, default)`.
    fn get_option_string(&self, name: &str, default_value: &str) -> String;
    /// `user_manager_->get_user_id_object(user_id, source)` — the i53
    /// surface the viewer builders read.
    fn get_user_id_object(&mut self, user_id: UserId, source: &str) -> i64;
    /// `user_manager_->have_user_force(user_id, source)` — the
    /// participant-list gate of `on_get_message_viewers`.
    fn have_user_force(&mut self, user_id: UserId, source: &str) -> bool;
    /// `send_closure(G()->td(), &Td::send_update, ...)`.
    fn send_update(&mut self, update: td_api::Update);

    // -- the view-metrics / read-metrics plane
    /// `Random::secure_int64` — the wire `view_id` draw.
    fn secure_random_i64(&self) -> i64;
    /// `dialog_manager_->check_dialog_access` — the M.6 dialog-plane gate;
    /// the collapsed line answers the not-found band (the full access
    /// check is the M.6/M.10 integration).
    fn check_dialog_access(
        &self,
        dialog_id: DialogId,
        _require_valid: bool,
        _access_rights: crate::user_manager::AccessRights,
        _source: &str,
    ) -> Result<(), tdutils::status::Status> {
        let _ = dialog_id;
        Err(tdutils::status::Status::error(400, "Chat not found"))
    }
    /// `dialog_manager_->have_input_peer` — the M.6 stub answers false
    /// (the full InputPeer family is the dialog-plane integration).
    fn have_input_peer(
        &self,
        _dialog_id: DialogId,
        _from_group_chat: bool,
        _access_rights: crate::user_manager::AccessRights,
    ) -> bool {
        false
    }
    /// `messages_manager_->have_message_force` — the
    /// [blocked: M.10 integration] probe.
    fn have_message_force(&mut self, _message_full_id: MessageFullId, _source: &str) -> bool {
        false
    }
    /// `messages_manager_->can_get_message_viewers` — the
    /// [blocked: M.10 integration] gate; answers the not-found band.
    fn can_get_message_viewers(
        &self,
        _message_full_id: MessageFullId,
    ) -> Result<(), tdutils::status::Status> {
        Err(tdutils::status::Status::error(400, "Message not found"))
    }
    /// `messages_manager_->get_messages_from_server` — the
    /// [blocked: M.10 integration] refetch.
    fn get_messages_from_server(&mut self, _message_full_ids: Vec<MessageFullId>) {}

    // -- the pts bookkeeping of the affected-history loop
    /// `updates_manager_->add_pending_pts_update` with the `dummyUpdate`.
    fn add_pending_pts_update(
        &mut self,
        _pts: i32,
        _pts_count: i32,
        _receive_time: f64,
        _promise: Promise<()>,
        _source: &str,
    ) {
    }
    /// `messages_manager_->add_pending_channel_update` — the
    /// [blocked: M.10 integration] channel-pts path.
    fn add_pending_channel_update(
        &mut self,
        _dialog_id: DialogId,
        _pts: i32,
        _pts_count: i32,
        _promise: Promise<()>,
        _source: &str,
    ) {
    }

    // -- the file-source plane
    /// `file_reference_manager_->create_rich_message_file_source` — the
    /// M.12 file-source registry stand-in.
    fn create_rich_message_file_source(
        &mut self,
        _message_full_id: MessageFullId,
    ) -> crate::files::FileSourceId {
        crate::files::FileSourceId::default()
    }

    // -- the paid-message-reaction sender load (the M.16 audit arm)
    /// `chat_manager_->load_created_public_broadcasts` + `get_me` — the
    /// [blocked: net] load half; the promise lands unanswered offline.
    fn load_paid_message_reaction_sender_state(
        &mut self,
        promise: Promise<td_api::MessageSenders>,
    ) {
        let _ = promise;
    }

    // -- the `[blocked: net]` query tails; each one owns the promise,
    // -- which keeps the C++ request unanswered offline
    /// The affected-history refill (the Delete*/Read*/Unpin* queries).
    fn send_affected_history_query(
        &mut self,
        _dialog_id: DialogId,
        _query: AffectedHistoryQueryKind,
        _get_affected_messages: bool,
        _promise: Promise<()>,
    ) {
    }
    /// `GetExtendedMediaQuery` (MessageQueryManager.cpp:360-394).
    fn send_get_extended_media_query(
        &mut self,
        _dialog_id: DialogId,
        _message_ids: Vec<MessageId>,
    ) {
    }
    /// `GetFactCheckQuery` (MessageQueryManager.cpp:396-430).
    fn send_get_fact_check_query(&mut self, _dialog_id: DialogId, _message_ids: Vec<MessageId>) {}
    /// `ReportReadMetricsQuery` (MessageQueryManager.cpp:1028-1049).
    fn send_report_read_metrics_query(
        &mut self,
        _dialog_id: DialogId,
        _metrics: Vec<MessageReadMetric>,
    ) {
    }
    /// `GetMessageReadParticipantsQuery` (MessageQueryManager.cpp:1096-1127).
    fn send_get_message_read_participants_query(
        &mut self,
        _message_full_id: MessageFullId,
        _promise: Promise<td_api::MessageViewers>,
    ) {
    }
    /// `GetMessagesViewsQuery` (MessageQueryManager.cpp:1129-1175).
    fn send_get_messages_views_query(
        &mut self,
        _dialog_id: DialogId,
        _message_ids: Vec<MessageId>,
        _increment_view_counter: bool,
    ) {
    }
    /// `GetMessagesReactionsQuery` (MessageQueryManager.cpp:1177-1229).
    fn send_get_messages_reactions_query(
        &mut self,
        _dialog_id: DialogId,
        _message_ids: Vec<MessageId>,
    ) {
    }
    /// `ReadMessagesContentsQuery`/`ReadChannelMessagesContentsQuery`
    /// (MessageQueryManager.cpp:2210-2289).
    fn send_read_message_contents_query(
        &mut self,
        _dialog_id: DialogId,
        _message_ids: Vec<MessageId>,
    ) {
    }
    /// `GetEmojiGameInfoQuery` (MessageQueryManager.cpp:1388-1414).
    fn send_get_emoji_game_info_query(&mut self, _promise: Promise<td_api::StakeDiceState>) {}

    // -- the request-slice seams (the M.mp manager methods) -----------------
    /// `messages_manager_->can_add_message_tasks` — the ToDoList-content
    /// check reads the loaded `Message *`; the collapsed line answers
    /// false (the not-loaded band, the M.10 message-storage integration).
    fn can_add_message_tasks(&mut self, _message_full_id: MessageFullId, _task_count: i32) -> bool {
        false
    }
    /// `messages_manager_->can_mark_message_tasks_as_done` — the same
    /// `Message *`-dependent gate.
    fn can_mark_message_tasks_as_done(&mut self, _message_full_id: MessageFullId) -> bool {
        false
    }
    /// `messages_manager_->get_message_requested_dialog_type` — the
    /// keyboardButtonSourceMessage resolution; the collapsed line answers
    /// the not-loaded band (400 "Message not found").
    fn get_message_requested_dialog_type(
        &mut self,
        _message_full_id: MessageFullId,
        _button_id: i32,
    ) -> Result<crate::requested_dialog_type::RequestedDialogType, tdutils::status::Status> {
        Err(tdutils::status::Status::error(400, "Message not found"))
    }
    /// `dialog_manager_->have_dialog_force` — the M.6 dialog-plane probe.
    fn have_dialog_force(&mut self, _dialog_id: DialogId, _source: &str) -> bool {
        false
    }
    /// `user_manager_->have_accessible_user` — the share plane gate.
    fn have_accessible_user(&mut self, _user_id: UserId) -> bool {
        false
    }
    /// `user_manager_->get_input_user` — the personal-history pre-step.
    fn get_input_user(
        &mut self,
        _user_id: UserId,
    ) -> Result<telegram_api::InputUser, tdutils::status::Status> {
        Err(tdutils::status::Status::error(400, "User not found"))
    }
    /// `messages_manager_->delete_local_dialog_messages_by_sender` — the
    /// local per-sender delete walk over the (not yet collapsed-in)
    /// dialog message store; a no-op default, the M.10 storage tail.
    fn delete_local_dialog_messages_by_sender(
        &mut self,
        _dialog_id: DialogId,
        _sender_dialog_id: DialogId,
    ) {
    }
    /// `messages_manager_->delete_local_dialog_messages_by_date` — the
    /// local by-date delete walk; the same storage tail.
    fn delete_local_dialog_messages_by_date(
        &mut self,
        _dialog_id: DialogId,
        _min_date: i32,
        _max_date: i32,
    ) {
    }
    /// `messages_manager_->on_delete_ephemeral_messages` — the local
    /// ephemeral-message removal (real over the updates sink in `Td`).
    fn on_delete_ephemeral_messages(
        &mut self,
        _dialog_id: DialogId,
        _ephemeral_message_ids: Vec<EphemeralMessageId>,
    ) {
    }
    /// `HashtagHints::hashtag_used` over `td_->hashtag_search_hints_` /
    /// `td_->cashtag_search_hints_` — the pmc-backed hints actor; the
    /// hints persistence is its own slice.
    fn hashtag_used(&mut self, _is_cashtag: bool, _hashtag: String) {}
    /// `td_->star_manager_->has_owned_star_count` — the StarManager seam
    /// answers true (the search proceeds), matching the payments-plane
    /// seam default.
    fn has_owned_star_count(&mut self, _star_count: i64) -> bool {
        true
    }

    // -- the `[blocked: net]` query tails of the request slice; each one
    // -- owns the promise, which keeps the C++ request unanswered offline
    /// `SearchMessagesGlobalQuery::send`
    /// (MessageQueryManager.cpp:3182-3204 payload).
    #[allow(clippy::too_many_arguments)]
    fn send_search_messages_global_query(
        &mut self,
        _folder_id: crate::messages_manager::FolderId,
        _ignore_folder_id: bool,
        _query: String,
        _offset_date: i32,
        _offset_dialog_id: DialogId,
        _offset_message_id: MessageId,
        _limit: i32,
        _filter: crate::messages_manager::MessageSearchFilter,
        _chat_type_filter: Option<td_api::SearchMessagesChatTypeFilter>,
        _min_date: i32,
        _max_date: i32,
        _promise: Promise<td_api::FoundMessages>,
    ) {
    }
    /// `SearchSentMediaQuery::send` (MessageQueryManager.cpp:3428 payload).
    fn send_search_sent_media_query(
        &mut self,
        _query: String,
        _limit: i32,
        _promise: Promise<td_api::FoundMessages>,
    ) {
    }
    /// `CheckSearchPostsFloodQuery::send`
    /// (MessageQueryManager.cpp:3439 payload).
    fn send_check_search_posts_flood_query(
        &mut self,
        _query: String,
        _promise: Promise<td_api::PublicPostSearchLimits>,
    ) {
    }
    /// `SearchPublicPostsQuery::send`
    /// (MessageQueryManager.cpp:3492 payload).
    fn send_search_public_posts_query(
        &mut self,
        _query: String,
        _offset: MessageSearchOffset,
        _limit: i32,
        _star_count: i64,
        _promise: Promise<td_api::FoundPublicPosts>,
    ) {
    }
    /// `SearchPostsQuery::send` (MessageQueryManager.cpp:3548 payload).
    fn send_search_posts_query(
        &mut self,
        _hashtag: String,
        _offset: MessageSearchOffset,
        _limit: i32,
        _promise: Promise<td_api::FoundMessages>,
    ) {
    }
    /// `GetRecentLocationsQuery::send`
    /// (MessageQueryManager.cpp:3605 payload).
    fn send_get_recent_locations_query(
        &mut self,
        _dialog_id: DialogId,
        _limit: i32,
        _promise: Promise<td_api::Messages>,
    ) {
    }
    /// `ReportMusicListenQuery::send`
    /// (MessageQueryManager.cpp:3679 payload).
    fn send_report_music_listen_query(
        &mut self,
        _input_document: telegram_api::InputDocument,
        _duration: i32,
        _promise: Promise<()>,
    ) {
    }
    /// `SummarizeTextQuery::send` (MessageQueryManager.cpp:3953 payload).
    fn send_summarize_text_query(
        &mut self,
        _dialog_id: DialogId,
        _message_id: MessageId,
        _to_language_code: String,
        _tone: String,
        _promise: Promise<td_api::FormattedText>,
    ) {
    }
    /// `AppendToDoListQuery::send`
    /// (MessageQueryManager.cpp:3970 payload).
    fn send_append_to_do_list_query(
        &mut self,
        _dialog_id: DialogId,
        _message_id: MessageId,
        _items: Vec<crate::message_content::ToDoItem>,
        _promise: Promise<()>,
    ) {
    }
    /// `ToggleToDoCompletedQuery::send`
    /// (MessageQueryManager.cpp:3982 payload).
    fn send_toggle_to_do_completed_query(
        &mut self,
        _dialog_id: DialogId,
        _message_id: MessageId,
        _done_task_ids: Vec<i32>,
        _not_done_task_ids: Vec<i32>,
        _promise: Promise<()>,
    ) {
    }
    /// `EditEphemeralMessageQuery::send`
    /// (MessageQueryManager.cpp:4106-4129 payload) — the text/caption and
    /// the media-upload completion funnel into the same query.
    #[allow(clippy::too_many_arguments)]
    fn send_edit_ephemeral_message_query(
        &mut self,
        _dialog_id: DialogId,
        _receiver_user_id: UserId,
        _ephemeral_message_id: EphemeralMessageId,
        _has_input_caption: bool,
        _input_caption: Option<crate::message_entity::FormattedText>,
        _disable_web_page_preview: bool,
        _invert_media: bool,
        _reply_markup: Option<crate::reply_markup::ReplyMarkup>,
        _promise: Promise<()>,
    ) {
    }
    /// the media-content leg of `edit_ephemeral_message` — the
    /// `create_upload_message_content_query` +
    /// `edit_ephemeral_message_queries_` plane (M.12 upload + net); the
    /// promise lands unanswered offline.
    #[allow(clippy::too_many_arguments)]
    fn start_edit_ephemeral_message_upload(
        &mut self,
        _dialog_id: DialogId,
        _receiver_user_id: UserId,
        _ephemeral_message_id: EphemeralMessageId,
        _reply_markup: Option<crate::reply_markup::ReplyMarkup>,
        _content: crate::message_content::MessageContent,
        _invert_media: bool,
        _promise: Promise<()>,
    ) {
    }
    /// `DeleteParticipantReactionsQuery::send`
    /// (MessageQueryManager.cpp:4800 payload).
    fn send_delete_participant_reactions_query(
        &mut self,
        _dialog_id: DialogId,
        _sender_dialog_id: DialogId,
        _promise: Promise<()>,
    ) {
    }
    /// `DeleteParticipantReactionQuery::send`
    /// (MessageQueryManager.cpp:4817 payload).
    fn send_delete_participant_reaction_query(
        &mut self,
        _dialog_id: DialogId,
        _message_id: MessageId,
        _sender_dialog_id: DialogId,
        _promise: Promise<()>,
    ) {
    }
    /// `GetPersonalChannelHistoryQuery::send`
    /// (MessageQueryManager.cpp:4830 payload).
    fn send_get_personal_channel_history_query(
        &mut self,
        _input_user: telegram_api::InputUser,
        _limit: i32,
        _promise: Promise<td_api::Messages>,
    ) {
    }
    /// `SendBotRequestedPeerQuery::send`
    /// (MessageQueryManager.cpp:3330 payload).
    #[allow(clippy::too_many_arguments)]
    fn send_send_bot_requested_peer_query(
        &mut self,
        _message_full_id: MessageFullId,
        _bot_user_id: UserId,
        _request_id: String,
        _button_id: i32,
        _shared_dialog_ids: Vec<DialogId>,
        _promise: Promise<()>,
    ) {
    }
    /// `DeleteEphemeralMessageQuery::send`
    /// (MessageQueryManager.cpp:4735 payload).
    fn send_delete_ephemeral_message_query(
        &mut self,
        _dialog_id: DialogId,
        _receiver_dialog_id: DialogId,
        _ephemeral_message_id: EphemeralMessageId,
        _promise: Promise<()>,
    ) {
    }

    // -- the parse pre-steps + remaining probes of the request slice -------
    /// `G()->is_test_dc()` — the switch-inline-button URL band.
    fn is_test_dc(&self) -> bool {
        false
    }
    /// `td_->inline_queries_manager_->get_requested_dialog_type` — the
    /// keyboardButtonSourceWebApp resolution over the prepared-button
    /// map (real over the InlineQueriesManager in `Td`).
    fn get_inline_requested_dialog_type(
        &mut self,
        _bot_user_id: UserId,
        _request_id: &str,
    ) -> Option<crate::requested_dialog_type::RequestedDialogType> {
        None
    }
    /// `dialog_manager_->can_delete_all_dialog_messages_by_sender` — the
    /// M.6 dialog-plane gate (real over the DialogManager in `Td`).
    fn can_delete_all_dialog_messages_by_sender(
        &mut self,
        _dialog_id: DialogId,
    ) -> Result<(), tdutils::status::Status> {
        Err(tdutils::status::Status::error(400, "Chat not found"))
    }
    /// the `FileManager::get_file_view` decision table of
    /// `report_music_listen` — `Some(as_input_document())` for the
    /// reportable documents, `None` for the silent `Unit()` bands (real
    /// over the FileManager in `Td`).
    fn get_music_listen_input_document(
        &mut self,
        _file_id: crate::files::FileId,
    ) -> Option<telegram_api::InputDocument> {
        None
    }
    /// `process_input_message_text(td_, dialog_id, ...)` — the
    /// `MessageContentContext` reads live on `Td`; unreachable while the
    /// access/eligibility seams gate the same requests first, real over
    /// the Td adapter.
    fn parse_input_message_text(
        &mut self,
        _dialog_id: DialogId,
        _input_message_content: td_api::InputMessageContent,
    ) -> Result<crate::message_content::InputMessageText, tdutils::status::Status> {
        Err(tdutils::status::Status::error(
            500,
            "The input parse is unreachable behind the offline seams",
        ))
    }
    /// `get_input_message_content(td_, dialog_id, ...)` — the same class.
    fn parse_input_message_content(
        &mut self,
        _dialog_id: DialogId,
        _input_message_content: td_api::InputMessageContent,
    ) -> Result<crate::message_content::InputMessageContent, tdutils::status::Status> {
        Err(tdutils::status::Status::error(
            500,
            "The input parse is unreachable behind the offline seams",
        ))
    }
    /// `get_formatted_text(td_, dialog_id, caption, is_bot, true, false,
    /// false)` — the caption parse pre-step of
    /// `edit_ephemeral_message_caption`.
    fn parse_input_caption(
        &mut self,
        _dialog_id: DialogId,
        _input_caption: Option<td_api::FormattedText>,
    ) -> Result<crate::message_entity::FormattedText, tdutils::status::Status> {
        Err(tdutils::status::Status::error(
            500,
            "The input parse is unreachable behind the offline seams",
        ))
    }
    /// `ToDoItem::get_to_do_item(td_, dialog_id, task)` — the checklist
    /// task parse pre-step of `add_to_do_list_tasks`.
    fn parse_to_do_item(
        &mut self,
        _dialog_id: DialogId,
        _task: td_api::InputChecklistTask,
    ) -> Result<crate::message_content::ToDoItem, tdutils::status::Status> {
        Err(tdutils::status::Status::error(
            500,
            "The input parse is unreachable behind the offline seams",
        ))
    }

    // -- the `RequestedDialogTypeEnv` probes of `check_shared_dialog` —
    // -- unreachable while the have_dialog_force/have_accessible_user
    // -- seams gate the same loop first; conservative defaults
    fn is_user_bot(&self, _user_id: UserId) -> bool {
        false
    }
    fn is_user_premium(&self, _user_id: UserId) -> bool {
        false
    }
    fn get_chat_is_active(&self, _chat_id: crate::chat_manager::ChatId) -> bool {
        false
    }
    fn get_chat_status(
        &self,
        _chat_id: crate::chat_manager::ChatId,
    ) -> crate::chat_manager::DialogParticipantStatus {
        Default::default()
    }
    fn is_broadcast_channel(&self, _channel_id: ChannelId) -> bool {
        false
    }
    fn is_monoforum_channel(&self, _channel_id: ChannelId) -> bool {
        false
    }
    fn is_forum_channel(&self, _channel_id: ChannelId) -> bool {
        false
    }
    fn get_channel_first_username(&self, _channel_id: ChannelId) -> String {
        String::new()
    }
    fn get_channel_status(
        &self,
        _channel_id: ChannelId,
    ) -> crate::chat_manager::DialogParticipantStatus {
        Default::default()
    }
    fn is_channel_public(&self, _channel_id: ChannelId) -> bool {
        false
    }
}

/// `MessageThreadInfo` (MessageThreadInfo.h) — the discussion-thread answer
/// `get_discussion_message` resolves to; the wire intake itself
/// (`process_discussion_message`) is the `[blocked: M.10 integration]`
/// half, the record is what survives the seam.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageThreadInfo {
    pub dialog_id: DialogId,
    pub message_ids: Vec<MessageId>,
    pub unread_message_count: i32,
}

/// `MessageViewer` (MessageViewer.h:20-41).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MessageViewer {
    user_id: UserId,
    date: i32,
}

impl MessageViewer {
    /// `MessageViewer(UserId, int32)` (MessageViewer.h:29-31) — the date is
    /// clamped to non-negative.
    pub fn new(user_id: UserId, date: i32) -> MessageViewer {
        MessageViewer {
            user_id,
            date: date.max(0),
        }
    }

    /// `MessageViewer(telegram_api::readParticipantDate)`
    /// (MessageViewer.cpp:16-18).
    pub fn from_read_participant_date(
        read_date: &telegram_api::ReadParticipantDate,
    ) -> MessageViewer {
        MessageViewer::new(UserId::new(read_date.user_id), read_date.date)
    }

    /// `get_user_id` (MessageViewer.h:32-34).
    pub fn get_user_id(&self) -> UserId {
        self.user_id
    }

    /// `is_empty` (MessageViewer.h:36-38).
    pub fn is_empty(&self) -> bool {
        self.user_id == UserId::default() && self.date == 0
    }

    /// `get_message_viewer_object` (MessageViewer.cpp:20-23).
    pub fn get_message_viewer_object(
        &self,
        env: &mut impl MessageQueryManagerEnv,
    ) -> td_api::MessageViewer {
        td_api::MessageViewer {
            user_id: env.get_user_id_object(self.user_id, "get_message_viewer_object"),
            view_date: self.date,
        }
    }
}

impl std::fmt::Display for MessageViewer {
    /// `operator<<(StringBuilder, MessageViewer)` (MessageViewer.cpp:25-27).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{} at {}]", self.user_id.get(), self.date)
    }
}

/// `MessageViewers` (MessageViewer.h:45-58).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MessageViewers {
    message_viewers: Vec<MessageViewer>,
}

impl MessageViewers {
    /// `MessageViewers(vector<readParticipantDate>)` (MessageViewer.cpp:29-38)
    /// — invalid user identifiers are dropped with the C++ `LOG(ERROR)`.
    pub fn from_read_participant_dates(
        read_dates: Vec<telegram_api::ReadParticipantDate>,
    ) -> MessageViewers {
        let mut message_viewers = Vec::with_capacity(read_dates.len());
        for read_date in read_dates {
            let viewer = MessageViewer::from_read_participant_date(&read_date);
            if !viewer.get_user_id().is_valid() {
                tracing::error!(
                    "Receive invalid {} as a viewer of a message",
                    viewer.get_user_id().get()
                );
                continue;
            }
            message_viewers.push(viewer);
        }
        MessageViewers { message_viewers }
    }

    /// `get_user_ids` (MessageViewer.cpp:40-42).
    pub fn get_user_ids(&self) -> Vec<UserId> {
        self.message_viewers
            .iter()
            .map(|viewer| viewer.get_user_id())
            .collect()
    }

    /// `get_message_viewers_object` (MessageViewer.cpp:44-49).
    pub fn get_message_viewers_object(
        &self,
        env: &mut impl MessageQueryManagerEnv,
    ) -> td_api::MessageViewers {
        td_api::MessageViewers {
            viewers: self
                .message_viewers
                .iter()
                .map(|viewer| viewer.get_message_viewer_object(env))
                .collect(),
        }
    }
}

impl std::fmt::Display for MessageViewers {
    /// `operator<<(StringBuilder, MessageViewers)` (MessageViewer.cpp:51-53).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let joined: Vec<String> = self
            .message_viewers
            .iter()
            .map(|viewer| viewer.to_string())
            .collect();
        write!(f, "[{}]", joined.join(", "))
    }
}

/// `AffectedHistory` (AffectedHistory.h) — the pts bookkeeping answer of
/// every history-changing query; `is_final_` is the `offset_ <= 0` fold.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AffectedHistory {
    pub pts: i32,
    pub pts_count: i32,
    pub is_final: bool,
}

impl AffectedHistory {
    /// `AffectedHistory(messages_affectedHistory)` (AffectedHistory.h:20-24).
    pub fn from_affected_history(
        affected_history: &telegram_api::messages_AffectedHistory,
    ) -> AffectedHistory {
        AffectedHistory {
            pts: affected_history.pts,
            pts_count: affected_history.pts_count,
            is_final: affected_history.offset <= 0,
        }
    }

    /// `AffectedHistory(messages_affectedFoundMessages)`
    /// (AffectedHistory.h:26-30).
    pub fn from_affected_found_messages(
        affected_history: &telegram_api::messages_AffectedFoundMessages,
    ) -> AffectedHistory {
        AffectedHistory {
            pts: affected_history.pts,
            pts_count: affected_history.pts_count,
            is_final: affected_history.offset <= 0,
        }
    }
}

/// `TonAmount::get_ton_count` (TonAmount.cpp:23-41) — the ±2⁵¹ nanogram
/// clamp; the StarManager twin (`StarManager::get_star_count`,
/// StarManager.cpp) is byte-identical, so both share this helper until the
/// owner managers land.
pub(crate) fn get_amount_count(amount: i64, allow_negative: bool) -> i64 {
    let max_amount = 1i64 << 51;
    if amount < 0 {
        if !allow_negative {
            tracing::error!("Receive amount = {amount}");
            return 0;
        }
        if amount < -max_amount {
            tracing::error!("Receive amount = {amount}");
            return -max_amount;
        }
    } else if amount > max_amount {
        tracing::error!("Receive amount = {amount}");
        return max_amount;
    }
    amount
}

/// `EmojiGameInfo` (EmojiGameInfo.h:18-34) — the dice-game cache entry; the
/// wire intake normalizes `messages_emojiGameDiceInfo` and rejects bad
/// shapes by falling back to the unavailable default.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EmojiGameInfo {
    game_hash: String,
    prev_stake: i64,
    current_streak: i32,
    params: Vec<i32>,
}

impl EmojiGameInfo {
    /// `EmojiGameInfo(messages_EmojiGameInfo)` (EmojiGameInfo.cpp:15-45).
    pub fn from_wire(game_info: &telegram_api::messages_EmojiGameInfo) -> EmojiGameInfo {
        let telegram_api::messages_EmojiGameInfo::messages_emojiGameDiceInfo(info) = game_info
        else {
            // `messages_emojiGameUnavailable` — the default record
            return EmojiGameInfo::default();
        };
        let mut is_bad = info.game_hash.is_empty()
            || info.prev_stake < 0
            || info.current_streak < 0
            || info.current_streak >= 3
            || info.params.len() != 7;
        for param in &info.params {
            if *param < 0 {
                is_bad = true;
            }
        }
        if is_bad {
            tracing::error!("Receive bad emojiGameDiceInfo");
            return EmojiGameInfo::default();
        }
        EmojiGameInfo {
            game_hash: info.game_hash.clone(),
            prev_stake: get_amount_count(info.prev_stake, false),
            current_streak: info.current_streak,
            params: info.params.clone(),
        }
    }

    /// `get_stake_dice_state_object` (EmojiGameInfo.cpp:47-61) — the
    /// suggested stake amounts come from the
    /// `ton_stakedice_stake_suggested_amounts` option; the C++
    /// `to_integer<int64>` CHECKs the parse, so does the port.
    pub fn get_stake_dice_state_object(
        &self,
        env: &impl MessageQueryManagerEnv,
    ) -> td_api::StakeDiceState {
        if self.game_hash.is_empty() {
            return td_api::StakeDiceState::default();
        }
        assert_eq!(self.params.len(), 7);
        let suggested_amounts: Vec<i64> = env
            .get_option_string("ton_stakedice_stake_suggested_amounts", "")
            .split(',')
            .map(|str| {
                str.parse::<i64>()
                    .expect("Failed to parse the suggested stake amount")
            })
            .collect();
        td_api::StakeDiceState {
            state_hash: self.game_hash.clone(),
            stake_gram_amount: self.prev_stake,
            suggested_stake_gram_amounts: suggested_amounts,
            current_streak: self.current_streak,
            prize_per_mille: self.params[..6].to_vec(),
            streak_prize_per_mille: self.params[6],
        }
    }

    /// `get_update_stake_dice_state_object` (EmojiGameInfo.cpp:63-65).
    pub fn get_update_stake_dice_state_object(
        &self,
        env: &impl MessageQueryManagerEnv,
    ) -> td_api::Update {
        td_api::Update::updateStakeDiceState(td_api::updateStakeDiceState {
            state: Some(self.get_stake_dice_state_object(env)),
        })
    }
}

/// `SearchPostsFlood` (SearchPostsFlood.h:17-36) — the public-post search
/// flood counters the search arms answer alongside the results.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SearchPostsFlood {
    total_daily: i32,
    remains: i32,
    star_count: i64,
    wait_till: i32,
    is_free: bool,
}

impl SearchPostsFlood {
    /// `SearchPostsFlood(searchPostsFlood)` (SearchPostsFlood.cpp:14-20) —
    /// the star count rides the ±2⁵¹ clamp.
    pub fn from_wire(flood: &telegram_api::SearchPostsFlood) -> SearchPostsFlood {
        SearchPostsFlood {
            total_daily: flood.total_daily,
            remains: flood.remains,
            star_count: get_amount_count(flood.stars_amount, false),
            wait_till: flood.wait_till,
            is_free: flood.query_is_free,
        }
    }

    /// `is_free` (SearchPostsFlood.h:33-35).
    pub fn is_free(&self) -> bool {
        self.is_free
    }

    /// `get_public_post_search_limits_object` (SearchPostsFlood.cpp:22-25).
    pub fn get_public_post_search_limits_object(
        &self,
        env: &impl MessageQueryManagerEnv,
    ) -> td_api::PublicPostSearchLimits {
        td_api::PublicPostSearchLimits {
            daily_free_query_count: self.total_daily,
            remaining_free_query_count: self.remains,
            next_free_query_in: (self.wait_till - env.unix_time()).max(0),
            star_count: self.star_count,
            is_current_query_free: self.is_free,
        }
    }
}

/// `MessagesManager::get_message_date` (MessagesManager.cpp) — the pure
/// wire date accessor the search offset needs; module-local until the
/// message plane lands.
fn get_message_date(message: &telegram_api::Message) -> i32 {
    match message {
        telegram_api::Message::messageEmpty(_) => 0,
        telegram_api::Message::message(message) => message.date.max(1),
        telegram_api::Message::messageService(message) => message.date.max(1),
    }
}

/// `MessageSearchOffset` (MessageSearchOffset.h:18-28) — the
/// `date,dialog_id,server_message_id` pagination token of the public-post
/// search arms.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MessageSearchOffset {
    date: i32,
    message_id: MessageId,
    dialog_id: DialogId,
}

impl MessageSearchOffset {
    /// `update_from_message` (MessageSearchOffset.cpp:21-30) — keeps the
    /// first usable message position.
    pub fn update_from_message(&mut self, message: &telegram_api::Message) {
        let message_date = get_message_date(message);
        let message_id = MessageId::get_message_id(message, false);
        let dialog_id = DialogId::get_message_dialog_id(message);
        if message_date > 0 && message_id.is_valid() && dialog_id.is_valid() {
            self.date = message_date;
            self.message_id = message_id;
            self.dialog_id = dialog_id;
        }
    }

    /// `to_string` (MessageSearchOffset.cpp:32-34) — the wire form.
    pub fn to_offset_string(&self) -> String {
        format!(
            "{},{},{}",
            self.date,
            self.dialog_id.get(),
            self.message_id.get_server_message_id().get()
        )
    }

    /// `from_string` (MessageSearchOffset.cpp:36-67) — the empty offset is
    /// the "no bound" form (`date = i32::MAX`, the ids left default); the
    /// invalid forms answer 400 "Invalid offset specified".
    pub fn from_offset_string(
        offset: &str,
    ) -> Result<MessageSearchOffset, tdutils::status::Status> {
        let mut result = MessageSearchOffset {
            date: i32::MAX,
            message_id: MessageId::default(),
            dialog_id: DialogId::default(),
        };
        let is_offset_valid = 'offset: {
            if offset.is_empty() {
                break 'offset true;
            }
            let parts: Vec<&str> = offset.split(',').collect();
            if parts.len() != 3 {
                break 'offset false;
            }
            let (Some(offset_date), Some(offset_dialog_id), Some(offset_message_id)) = (
                parts.first().and_then(|part| part.parse::<i32>().ok()),
                parts.get(1).and_then(|part| part.parse::<i64>().ok()),
                parts.get(2).and_then(|part| part.parse::<i32>().ok()),
            ) else {
                break 'offset false;
            };
            result.date = offset_date;
            result.message_id =
                MessageId::from_server_message_id(ServerMessageId::new(offset_message_id));
            result.dialog_id = DialogId::new(offset_dialog_id);
            if result.date <= 0
                || !result.message_id.is_valid()
                || !result.dialog_id.is_valid()
                || matches!(
                    crate::dialog_manager::get_input_peer_force(result.dialog_id),
                    telegram_api::InputPeer::inputPeerEmpty(_)
                )
            {
                break 'offset false;
            }
            true
        };
        if !is_offset_valid {
            return Err(tdutils::status::Status::error(
                400,
                "Invalid offset specified",
            ));
        }
        Ok(result)
    }
}

/// `LogEvent::HandlerType` — the M.18-owned binlog handler entries
/// (LogEvent.h). The Rust binlog plane keys logevents by name; the numeric
/// C++ identities are kept in the docs for the eventual database step.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MessageQueryLogEventType {
    /// `BlockMessageSenderFromRepliesOnServer`.
    BlockMessageSenderFromRepliesOnServer,
    /// `DeleteAllCallMessagesOnServer`.
    DeleteAllCallMessagesOnServer,
    /// `DeleteAllChannelMessagesFromSenderOnServer`.
    DeleteAllChannelMessagesFromSenderOnServer,
    /// `DeleteDialogHistoryOnServer`.
    DeleteDialogHistoryOnServer,
    /// `DeleteDialogMessagesByDateOnServer`.
    DeleteDialogMessagesByDateOnServer,
    /// `DeleteMessagesOnServer`.
    DeleteMessagesOnServer,
    /// `DeleteScheduledMessagesOnServer`.
    DeleteScheduledMessagesOnServer,
    /// `DeleteEphemeralMessageOnServer`.
    DeleteEphemeralMessageOnServer,
    /// `DeleteTopicHistoryOnServer`.
    DeleteTopicHistoryOnServer,
    /// `ReadAllDialogMentionsOnServer`.
    ReadAllDialogMentionsOnServer,
    /// `ReadAllDialogReactionsOnServer`.
    ReadAllDialogReactionsOnServer,
    /// `ReadAllPollVotesOnServer`.
    ReadAllPollVotesOnServer,
    /// `ReadMessageContentsOnServer`.
    ReadMessageContentsOnServer,
    /// `UnpinAllDialogMessagesOnServer`.
    UnpinAllDialogMessagesOnServer,
}

impl std::fmt::Display for MessageQueryLogEventType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            MessageQueryLogEventType::BlockMessageSenderFromRepliesOnServer => {
                "BlockMessageSenderFromRepliesOnServer"
            }
            MessageQueryLogEventType::DeleteAllCallMessagesOnServer => {
                "DeleteAllCallMessagesOnServer"
            }
            MessageQueryLogEventType::DeleteAllChannelMessagesFromSenderOnServer => {
                "DeleteAllChannelMessagesFromSenderOnServer"
            }
            MessageQueryLogEventType::DeleteDialogHistoryOnServer => "DeleteDialogHistoryOnServer",
            MessageQueryLogEventType::DeleteDialogMessagesByDateOnServer => {
                "DeleteDialogMessagesByDateOnServer"
            }
            MessageQueryLogEventType::DeleteMessagesOnServer => "DeleteMessagesOnServer",
            MessageQueryLogEventType::DeleteScheduledMessagesOnServer => {
                "DeleteScheduledMessagesOnServer"
            }
            MessageQueryLogEventType::DeleteEphemeralMessageOnServer => {
                "DeleteEphemeralMessageOnServer"
            }
            MessageQueryLogEventType::DeleteTopicHistoryOnServer => "DeleteTopicHistoryOnServer",
            MessageQueryLogEventType::ReadAllDialogMentionsOnServer => {
                "ReadAllDialogMentionsOnServer"
            }
            MessageQueryLogEventType::ReadAllDialogReactionsOnServer => {
                "ReadAllDialogReactionsOnServer"
            }
            MessageQueryLogEventType::ReadAllPollVotesOnServer => "ReadAllPollVotesOnServer",
            MessageQueryLogEventType::ReadMessageContentsOnServer => "ReadMessageContentsOnServer",
            MessageQueryLogEventType::UnpinAllDialogMessagesOnServer => {
                "UnpinAllDialogMessagesOnServer"
            }
        };
        write!(f, "{name}")
    }
}

/// The per-type structs below are the byte-exact `store`/`parse` ports of
/// the `MessageQueryManager::*OnServerLogEvent` family
/// (MessageQueryManager.cpp:4079-5142). Every block pins the C++ field
/// order and the flag-block layout, including the deliberately empty
/// `DeleteTopicHistory` flags word.
///
/// `BlockMessageSenderFromRepliesOnServerLogEvent` (cpp:4079-4107).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BlockMessageSenderFromRepliesOnServerLogEvent {
    pub message_id: MessageId,
    pub delete_message: bool,
    pub delete_all_messages: bool,
    pub report_spam: bool,
}

impl BlockMessageSenderFromRepliesOnServerLogEvent {
    pub fn store(&self, out: &mut Vec<u8>) {
        store_parse::store_flags(
            out,
            [
                self.delete_message,
                self.delete_all_messages,
                self.report_spam,
            ],
        );
        self.message_id.store(out);
    }

    pub fn parse(parser: &mut Parser<'_>) -> Result<Self, &'static str> {
        let mut result = Self::default();
        let [delete_message, delete_all_messages, report_spam] = parser.parse_flags()?;
        result.delete_message = delete_message;
        result.delete_all_messages = delete_all_messages;
        result.report_spam = report_spam;
        result.message_id = MessageId::parse_from(parser)?;
        Ok(result)
    }
}

/// `DeleteAllCallMessagesOnServerLogEvent` (cpp:4303-4320).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DeleteAllCallMessagesOnServerLogEvent {
    pub revoke: bool,
}

impl DeleteAllCallMessagesOnServerLogEvent {
    pub fn store(&self, out: &mut Vec<u8>) {
        store_parse::store_flags(out, [self.revoke]);
    }

    pub fn parse(parser: &mut Parser<'_>) -> Result<Self, &'static str> {
        let mut result = Self::default();
        let [revoke] = parser.parse_flags()?;
        result.revoke = revoke;
        Ok(result)
    }
}

/// `DeleteAllChannelMessagesFromSenderOnServerLogEvent` (cpp:4341-4363) —
/// the sender was a bare user id before `Version::AddKeyboardButtonFlags`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DeleteAllChannelMessagesFromSenderOnServerLogEvent {
    pub channel_id: ChannelId,
    pub sender_dialog_id: DialogId,
}

impl DeleteAllChannelMessagesFromSenderOnServerLogEvent {
    pub fn store(&self, out: &mut Vec<u8>) {
        store_parse::store_i64(out, self.channel_id.get());
        self.sender_dialog_id.store(out);
    }

    pub fn parse(parser: &mut Parser<'_>) -> Result<Self, &'static str> {
        let mut result = Self {
            channel_id: ChannelId::new(parser.i64_()?),
            ..Default::default()
        };
        if parser.version() >= store_parse::version::ADD_KEYBOARD_BUTTON_FLAGS {
            result.sender_dialog_id = DialogId::parse_from(parser)?;
        } else {
            // `UserId user_id; td::parse(user_id, parser)` — the raw i64
            // form (UserId.h store/parse)
            let user_id = UserId::new(parser.i64_()?);
            result.sender_dialog_id = DialogId::from_user_id(user_id);
        }
        Ok(result)
    }
}

/// `DeleteDialogHistoryOnServerLogEvent` (cpp:4390-4418).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DeleteDialogHistoryOnServerLogEvent {
    pub dialog_id: DialogId,
    pub max_message_id: MessageId,
    pub remove_from_dialog_list: bool,
    pub revoke: bool,
}

impl DeleteDialogHistoryOnServerLogEvent {
    pub fn store(&self, out: &mut Vec<u8>) {
        store_parse::store_flags(out, [self.remove_from_dialog_list, self.revoke]);
        self.dialog_id.store(out);
        self.max_message_id.store(out);
    }

    pub fn parse(parser: &mut Parser<'_>) -> Result<Self, &'static str> {
        let mut result = Self::default();
        let [remove_from_dialog_list, revoke] = parser.parse_flags()?;
        result.remove_from_dialog_list = remove_from_dialog_list;
        result.revoke = revoke;
        result.dialog_id = DialogId::parse_from(parser)?;
        result.max_message_id = MessageId::parse_from(parser)?;
        Ok(result)
    }
}

/// `DeleteDialogMessagesByDateOnServerLogEvent` (cpp:4466-4492).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DeleteDialogMessagesByDateOnServerLogEvent {
    pub dialog_id: DialogId,
    pub min_date: i32,
    pub max_date: i32,
    pub revoke: bool,
}

impl DeleteDialogMessagesByDateOnServerLogEvent {
    pub fn store(&self, out: &mut Vec<u8>) {
        store_parse::store_flags(out, [self.revoke]);
        self.dialog_id.store(out);
        store_parse::store_i32(out, self.min_date);
        store_parse::store_i32(out, self.max_date);
    }

    pub fn parse(parser: &mut Parser<'_>) -> Result<Self, &'static str> {
        let mut result = Self::default();
        let [revoke] = parser.parse_flags()?;
        result.revoke = revoke;
        result.dialog_id = DialogId::parse_from(parser)?;
        result.min_date = parser.i32_()?;
        result.max_date = parser.i32_()?;
        Ok(result)
    }
}

/// `DeleteMessagesOnServerLogEvent` (cpp:4545-4570).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeleteMessagesOnServerLogEvent {
    pub dialog_id: DialogId,
    pub message_ids: Vec<MessageId>,
    pub revoke: bool,
}

impl DeleteMessagesOnServerLogEvent {
    pub fn store(&self, out: &mut Vec<u8>) {
        store_parse::store_flags(out, [self.revoke]);
        self.dialog_id.store(out);
        store_parse::store_i32(out, self.message_ids.len() as i32);
        for message_id in &self.message_ids {
            message_id.store(out);
        }
    }

    pub fn parse(parser: &mut Parser<'_>) -> Result<Self, &'static str> {
        let mut result = Self::default();
        let [revoke] = parser.parse_flags()?;
        result.revoke = revoke;
        result.dialog_id = DialogId::parse_from(parser)?;
        let len = parser.i32_()?;
        if len < 0 {
            return Err("Invalid vector size");
        }
        result.message_ids.reserve(len as usize);
        for _ in 0..len {
            result.message_ids.push(MessageId::parse_from(parser)?);
        }
        Ok(result)
    }
}

/// `DeleteScheduledMessagesOnServerLogEvent` (cpp:4644-4660) — no flag
/// block, unlike the non-scheduled twin.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeleteScheduledMessagesOnServerLogEvent {
    pub dialog_id: DialogId,
    pub message_ids: Vec<MessageId>,
}

impl DeleteScheduledMessagesOnServerLogEvent {
    pub fn store(&self, out: &mut Vec<u8>) {
        self.dialog_id.store(out);
        store_parse::store_i32(out, self.message_ids.len() as i32);
        for message_id in &self.message_ids {
            message_id.store(out);
        }
    }

    pub fn parse(parser: &mut Parser<'_>) -> Result<Self, &'static str> {
        let mut result = Self {
            dialog_id: DialogId::parse_from(parser)?,
            ..Default::default()
        };
        let len = parser.i32_()?;
        if len < 0 {
            return Err("Invalid vector size");
        }
        result.message_ids.reserve(len as usize);
        for _ in 0..len {
            result.message_ids.push(MessageId::parse_from(parser)?);
        }
        Ok(result)
    }
}

/// `DeleteEphemeralMessageOnServerLogEvent` (cpp:4686-4705).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DeleteEphemeralMessageOnServerLogEvent {
    pub dialog_id: DialogId,
    pub receiver_dialog_id: DialogId,
    pub ephemeral_message_id: i32,
}

impl DeleteEphemeralMessageOnServerLogEvent {
    pub fn store(&self, out: &mut Vec<u8>) {
        self.dialog_id.store(out);
        self.receiver_dialog_id.store(out);
        store_parse::store_i32(out, self.ephemeral_message_id);
    }

    pub fn parse(parser: &mut Parser<'_>) -> Result<Self, &'static str> {
        let mut result = Self {
            dialog_id: DialogId::parse_from(parser)?,
            ..Default::default()
        };
        result.receiver_dialog_id = DialogId::parse_from(parser)?;
        result.ephemeral_message_id = parser.i32_()?;
        Ok(result)
    }
}

/// `DeleteTopicHistoryOnServerLogEvent` (cpp:4745-4765) — the empty
/// `BEGIN_STORE_FLAGS`/`END_STORE_FLAGS` block still writes the zero
/// `uint32`; kept byte-exact.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DeleteTopicHistoryOnServerLogEvent {
    pub dialog_id: DialogId,
    pub forum_topic_id: crate::files::ForumTopicId,
}

impl DeleteTopicHistoryOnServerLogEvent {
    pub fn store(&self, out: &mut Vec<u8>) {
        store_parse::store_flags(out, []);
        self.dialog_id.store(out);
        self.forum_topic_id.store(out);
    }

    pub fn parse(parser: &mut Parser<'_>) -> Result<Self, &'static str> {
        let [] = parser.parse_flags()?;
        Ok(Self {
            dialog_id: DialogId::parse_from(parser)?,
            forum_topic_id: crate::files::ForumTopicId::parse_from(parser)?,
        })
    }
}

/// `ReadAllDialogMentionsOnServerLogEvent` (cpp:4839-4852).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReadAllDialogMentionsOnServerLogEvent {
    pub dialog_id: DialogId,
}

impl ReadAllDialogMentionsOnServerLogEvent {
    pub fn store(&self, out: &mut Vec<u8>) {
        self.dialog_id.store(out);
    }

    pub fn parse(parser: &mut Parser<'_>) -> Result<Self, &'static str> {
        Ok(Self {
            dialog_id: DialogId::parse_from(parser)?,
        })
    }
}

/// `ReadAllDialogReactionsOnServerLogEvent` (cpp:4884-4897).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReadAllDialogReactionsOnServerLogEvent {
    pub dialog_id: DialogId,
}

impl ReadAllDialogReactionsOnServerLogEvent {
    pub fn store(&self, out: &mut Vec<u8>) {
        self.dialog_id.store(out);
    }

    pub fn parse(parser: &mut Parser<'_>) -> Result<Self, &'static str> {
        Ok(Self {
            dialog_id: DialogId::parse_from(parser)?,
        })
    }
}

/// `ReadAllPollVotesOnServerLogEvent` (cpp:4919-4947) — the topic id is
/// stored only when valid, with the presence flag in the flag block.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReadAllPollVotesOnServerLogEvent {
    pub dialog_id: DialogId,
    pub forum_topic_id: crate::files::ForumTopicId,
}

impl ReadAllPollVotesOnServerLogEvent {
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_forum_topic_id = self.forum_topic_id.is_valid();
        store_parse::store_flags(out, [has_forum_topic_id]);
        self.dialog_id.store(out);
        if has_forum_topic_id {
            self.forum_topic_id.store(out);
        }
    }

    pub fn parse(parser: &mut Parser<'_>) -> Result<Self, &'static str> {
        let [has_forum_topic_id] = parser.parse_flags()?;
        let mut result = Self {
            dialog_id: DialogId::parse_from(parser)?,
            ..Default::default()
        };
        if has_forum_topic_id {
            result.forum_topic_id = crate::files::ForumTopicId::parse_from(parser)?;
        }
        Ok(result)
    }
}

/// `ReadMessageContentsOnServerLogEvent` (cpp:4981-4997).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReadMessageContentsOnServerLogEvent {
    pub dialog_id: DialogId,
    pub message_ids: Vec<MessageId>,
}

impl ReadMessageContentsOnServerLogEvent {
    pub fn store(&self, out: &mut Vec<u8>) {
        self.dialog_id.store(out);
        store_parse::store_i32(out, self.message_ids.len() as i32);
        for message_id in &self.message_ids {
            message_id.store(out);
        }
    }

    pub fn parse(parser: &mut Parser<'_>) -> Result<Self, &'static str> {
        let mut result = Self {
            dialog_id: DialogId::parse_from(parser)?,
            ..Default::default()
        };
        let len = parser.i32_()?;
        if len < 0 {
            return Err("Invalid vector size");
        }
        result.message_ids.reserve(len as usize);
        for _ in 0..len {
            result.message_ids.push(MessageId::parse_from(parser)?);
        }
        Ok(result)
    }
}

/// `UnpinAllDialogMessagesOnServerLogEvent` (cpp:5123-5136).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UnpinAllDialogMessagesOnServerLogEvent {
    pub dialog_id: DialogId,
}

impl UnpinAllDialogMessagesOnServerLogEvent {
    pub fn store(&self, out: &mut Vec<u8>) {
        self.dialog_id.store(out);
    }

    pub fn parse(parser: &mut Parser<'_>) -> Result<Self, &'static str> {
        Ok(Self {
            dialog_id: DialogId::parse_from(parser)?,
        })
    }
}

/// `MessageQueryManager` (MessageQueryManager.h:58-585) — the collapsed
/// actor state. The upload-content/cover machinery and the rich-message
/// query family stay behind their seams (Files/net/M.17); the pending
/// read counters, the reactions-reload queue, the view-metrics buffer,
/// the view dedup sets, the affected-history loop and the emoji-game
/// cache are the local machine the manager exists for.
#[derive(Default)]
pub struct MessageQueryManager {
    /// `being_reloaded_extended_media_message_full_ids_` — dedup only; the
    /// C++ `reload_message_extended_media` inserts into the fact-check
    /// set (MessageQueryManager.cpp:3264-3265) and the finish erases from
    /// this one (cpp:3273-3277) — mirrored as-is.
    being_reloaded_extended_media_message_full_ids: HashSet<MessageFullId>,
    /// `being_reloaded_fact_checks_`.
    being_reloaded_fact_checks: HashSet<MessageFullId>,

    /// `need_view_counter_increment_message_full_ids_`.
    need_view_counter_increment_message_full_ids: HashSet<MessageFullId>,
    /// `being_reloaded_views_message_full_ids_`.
    being_reloaded_views_message_full_ids: HashSet<MessageFullId>,

    /// `ReactionsToReload` + `being_reloaded_reactions_`
    /// (MessageQueryManager.h:503-507).
    being_reloaded_reactions: HashMap<DialogId, ReactionsToReload>,
    /// The `SleepActor(0.2)` retry stand-in
    /// (MessageQueryManager.cpp:3878-3884): the dialog fires
    /// `try_reload_message_reactions(dialog, true)` at the deadline —
    /// the collapsed-line deadline replaces the one-shot actor.
    reactions_reload_retry_deadlines: HashMap<DialogId, f64>,

    /// `pending_read_reactions_` (MessageQueryManager.h:509).
    pending_read_reactions: HashMap<MessageFullId, i32>,
    /// `pending_read_poll_votes_` (MessageQueryManager.h:511).
    pending_read_poll_votes: HashMap<MessageFullId, i32>,

    /// `MessageViewMetrics` + `pending_message_view_metrics_`
    /// (MessageQueryManager.h:513-520).
    pending_message_view_metrics: HashMap<DialogId, Vec<MessageViewMetrics>>,
    /// The `send_message_view_metrics_timeout_` MultiTimeout
    /// (MessageQueryManager.h:562): dialog → 5.0s deadline.
    view_metrics_deadlines: HashMap<DialogId, f64>,

    /// `rich_message_full_id_to_file_source_id_`
    /// (MessageQueryManager.h:560) — the WaitFreeHashMap collapses to a
    /// plain map.
    rich_message_full_id_to_file_source_id: HashMap<MessageFullId, crate::files::FileSourceId>,
    /// `is_emoji_game_info_inited_` (MessageQueryManager.h:552-554).
    is_emoji_game_info_inited: bool,
    emoji_game_info_receive_time: f64,
    emoji_game_info: EmojiGameInfo,
}

/// `MessageQueryManager::ReactionsToReload` (MessageQueryManager.h:503-506).
#[derive(Default)]
struct ReactionsToReload {
    message_ids: HashSet<MessageId>,
    is_request_sent: bool,
}

/// `MessageQueryManager::MessageViewMetrics` (MessageQueryManager.h:513-519).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct MessageViewMetrics {
    message_id: MessageId,
    time_in_view_ms: i32,
    active_time_in_view_ms: i32,
    height_to_viewport_ratio_per_mille: i32,
    seen_range_ratio_per_mille: i32,
}

/// The `telegram_api::inputMessageReadMetric` payload the timeout flush
/// sends — the Rust stand-in for the wire struct (the id draw rides the
/// env's secure random).
pub struct MessageReadMetric {
    pub message_id: MessageId,
    pub view_id: i64,
    pub time_in_view_ms: i32,
    pub active_time_in_view_ms: i32,
    pub height_to_viewport_ratio_per_mille: i32,
    pub seen_range_ratio_per_mille: i32,
}

/// The affected-history query identity — which net request refills the
/// loop ([blocked: net]; the C++ closures at
/// MessageQueryManager.cpp:4334/4380/4443/4536/4783/4832/4866/4876/4911/4962/4973/5150).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AffectedHistoryQueryKind {
    DeleteAllCallMessages {
        revoke: bool,
    },
    DeleteAllChannelMessagesFromSender {
        sender_dialog_id: DialogId,
    },
    DeleteDialogHistory {
        max_message_id: MessageId,
        remove_from_dialog_list: bool,
        revoke: bool,
    },
    DeleteDialogMessagesByDate {
        min_date: i32,
        max_date: i32,
        revoke: bool,
    },
    DeleteTopicHistory {
        forum_topic_id: crate::files::ForumTopicId,
    },
    ReadAllDialogMentions,
    ReadAllTopicMentions {
        forum_topic_id: crate::files::ForumTopicId,
    },
    ReadAllDialogReactions,
    ReadAllTopicReactions {
        forum_topic_id: crate::files::ForumTopicId,
    },
    ReadAllDialogPollVotes {
        forum_topic_id: crate::files::ForumTopicId,
    },
    UnpinAllDialogMessages,
    UnpinAllTopicMessages {
        forum_topic_id: crate::files::ForumTopicId,
    },
}

impl MessageQueryManager {
    pub fn new() -> MessageQueryManager {
        MessageQueryManager::default()
    }

    /// `fix_delete_message_min_max_dates` (MessageQueryManager.cpp:4501-4527)
    /// — the static date-window normalizer; returns the fixed
    /// `(min_date, max_date)` pair.
    pub fn fix_delete_message_min_max_dates(
        env: &impl MessageQueryManagerEnv,
        mut min_date: i32,
        mut max_date: i32,
    ) -> Result<(i32, i32), tdutils::status::Status> {
        if min_date > max_date {
            return Err(tdutils::status::Status::error(
                400,
                "Wrong date interval specified",
            ));
        }

        const TELEGRAM_LAUNCH_DATE: i32 = 1376438400;
        if max_date < TELEGRAM_LAUNCH_DATE {
            return Ok((0, 0));
        }
        if min_date < TELEGRAM_LAUNCH_DATE {
            min_date = TELEGRAM_LAUNCH_DATE;
        }

        let current_date = env.unix_time().max(1635000000);
        if min_date >= current_date - 30 {
            return Ok((0, 0));
        }
        if max_date >= current_date - 30 {
            max_date = current_date - 31;
        }
        assert!(min_date <= max_date);
        Ok((min_date, max_date))
    }

    /// `run_affected_history_query_until_complete`
    /// (MessageQueryManager.cpp:2447-2461). The query send is the
    /// `[blocked: net]` tail — the promise lands in the query and stays
    /// unanswered offline; the net step routes each
    /// `messages_affectedHistory` back into [`Self::on_get_affected_history`].
    pub fn run_affected_history_query_until_complete(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        dialog_id: DialogId,
        query: AffectedHistoryQueryKind,
        get_affected_messages: bool,
        promise: Promise<()>,
    ) {
        assert!(!env.close_flag());
        env.send_affected_history_query(dialog_id, query, get_affected_messages, promise);
    }

    /// `on_get_affected_history` (MessageQueryManager.cpp:2463-2492) — the
    /// pts bookkeeping and the loop continuation.
    pub fn on_get_affected_history(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        dialog_id: DialogId,
        query: AffectedHistoryQueryKind,
        get_affected_messages: bool,
        mut affected_history: AffectedHistory,
        mut promise: Promise<()>,
    ) {
        if env.close_flag() {
            return promise.set_error(tdutils::status::Status::error(500, "Request aborted"));
        }
        tracing::info!(
            "Receive {} affected history with PTS = {} and pts_count = {}",
            if affected_history.is_final {
                "final"
            } else {
                "partial"
            },
            affected_history.pts,
            affected_history.pts_count
        );

        if affected_history.pts_count > 0 {
            if get_affected_messages {
                affected_history.pts_count = 0;
            }
            let update_promise = if affected_history.is_final {
                std::mem::take(&mut promise)
            } else {
                Promise::default()
            };
            if dialog_id.get_type() == crate::dialog_id::DialogType::Channel {
                env.add_pending_channel_update(
                    dialog_id,
                    affected_history.pts,
                    affected_history.pts_count,
                    update_promise,
                    "on_get_affected_history",
                );
            } else {
                let receive_time = env.now() - if get_affected_messages { 10.0 } else { 0.0 };
                env.add_pending_pts_update(
                    affected_history.pts,
                    affected_history.pts_count,
                    receive_time,
                    update_promise,
                    "on_get_affected_history",
                );
            }
        } else if affected_history.is_final {
            promise.set_value(());
        }

        if !affected_history.is_final {
            self.run_affected_history_query_until_complete(
                env,
                dialog_id,
                query,
                get_affected_messages,
                promise,
            );
        }
    }

    /// `get_rich_message_file_source_id` (MessageQueryManager.cpp:2551-2567)
    /// — the FileSource cache half; the source creation rides the
    /// FileReferenceManager seam.
    pub fn get_rich_message_file_source_id(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        message_full_id: MessageFullId,
    ) -> crate::files::FileSourceId {
        if env.auth_is_bot() {
            return crate::files::FileSourceId::default();
        }

        let dialog_id = message_full_id.get_dialog_id();
        let message_id = message_full_id.get_message_id();
        if !dialog_id.is_valid()
            || dialog_id.get_type() == crate::dialog_id::DialogType::SecretChat
            || !message_id.is_server()
        {
            return crate::files::FileSourceId::default();
        }

        let entry = self
            .rich_message_full_id_to_file_source_id
            .entry(message_full_id)
            .or_default();
        if !entry.is_valid() {
            *entry = env.create_rich_message_file_source(message_full_id);
        }
        *entry
    }

    /// `reload_message_extended_media` (MessageQueryManager.cpp:3262-3271)
    /// — the dedup half; the query send is `[blocked: net]`.
    pub fn reload_message_extended_media(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        dialog_id: DialogId,
        mut message_ids: Vec<MessageId>,
    ) {
        assert!(dialog_id.get_type() != crate::dialog_id::DialogType::SecretChat);
        // the C++ inserts into being_reloaded_fact_checks_ here
        // (cpp:3264-3265) — mirrored as-is
        message_ids.retain(|message_id| {
            self.being_reloaded_fact_checks
                .insert(MessageFullId::new(dialog_id, *message_id))
        });
        if message_ids.is_empty() {
            return;
        }
        env.send_get_extended_media_query(dialog_id, message_ids);
    }

    /// `finish_get_message_extended_media`
    /// (MessageQueryManager.cpp:3273-3277).
    pub fn finish_get_message_extended_media(
        &mut self,
        dialog_id: DialogId,
        message_ids: &[MessageId],
    ) {
        for message_id in message_ids {
            self.being_reloaded_extended_media_message_full_ids
                .remove(&MessageFullId::new(dialog_id, *message_id));
        }
    }

    /// `reload_message_fact_checks` (MessageQueryManager.cpp:3279-3295) —
    /// the dedup half; the query send is `[blocked: net]`.
    pub fn reload_message_fact_checks(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        dialog_id: DialogId,
        mut message_ids: Vec<MessageId>,
    ) {
        assert!(dialog_id.get_type() != crate::dialog_id::DialogType::SecretChat);
        message_ids.retain(|message_id| {
            self.being_reloaded_fact_checks
                .insert(MessageFullId::new(dialog_id, *message_id))
        });
        if message_ids.is_empty() {
            return;
        }
        env.send_get_fact_check_query(dialog_id, message_ids);
    }

    /// `on_reload_message_fact_checks` (MessageQueryManager.cpp:3297-3318)
    /// — the local bookkeeping; the fact-check intake lands with the
    /// message plane.
    pub fn on_reload_message_fact_checks(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        dialog_id: DialogId,
        message_ids: &[MessageId],
        is_error: bool,
    ) {
        for message_id in message_ids {
            let erased = self
                .being_reloaded_fact_checks
                .remove(&MessageFullId::new(dialog_id, *message_id));
            assert!(erased);
        }
        let _ = (is_error, env);
        // `on_update_message_fact_check` — the [blocked: M.10 integration]
        // consumer
    }

    /// `send_message_view_metrics` (MessageQueryManager.cpp:3652-3678) —
    /// validation, the local accumulate and the 5.0s re-arm; the flush is
    /// the `[blocked: net]` tail.
    #[allow(clippy::too_many_arguments)]
    pub fn send_message_view_metrics(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        dialog_id: DialogId,
        message_id: MessageId,
        time_in_view_ms: i32,
        active_time_in_view_ms: i32,
        height_to_viewport_ratio_per_mille: i32,
        seen_range_ratio_per_mille: i32,
        promise: Promise<()>,
    ) {
        let mut promise = promise;
        if let Err(error) = env.check_dialog_access(
            dialog_id,
            true,
            crate::user_manager::AccessRights::Read,
            "send_message_view_metrics",
        ) {
            return promise.set_error(error);
        }
        if !(0..=1000000000).contains(&time_in_view_ms)
            || !(0..=1000000000).contains(&active_time_in_view_ms)
            || height_to_viewport_ratio_per_mille < 0
            || !(0..=1000).contains(&seen_range_ratio_per_mille)
        {
            return promise.set_error(tdutils::status::Status::error(
                400,
                "Invalid metrics specified",
            ));
        }
        if dialog_id.get_type() == crate::dialog_id::DialogType::SecretChat
            || !message_id.is_server()
            || time_in_view_ms < 300
            || !env.have_message_force(
                MessageFullId::new(dialog_id, message_id),
                "send_message_view_metrics",
            )
        {
            return promise.set_value(());
        }
        self.pending_message_view_metrics
            .entry(dialog_id)
            .or_default()
            .push(MessageViewMetrics {
                message_id,
                time_in_view_ms,
                active_time_in_view_ms,
                height_to_viewport_ratio_per_mille,
                seen_range_ratio_per_mille,
            });
        self.view_metrics_deadlines
            .insert(dialog_id, env.now() + 5.0);

        promise.set_value(());
    }

    /// `send_message_view_metrics_timeout`
    /// (MessageQueryManager.cpp:3691-3723) — the fold: summed view times,
    /// max ratios, one metric per message; the report send is
    /// `[blocked: net]`.
    fn send_message_view_metrics_timeout(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        dialog_id: DialogId,
    ) {
        let Some(input_metrics_raw) = self.pending_message_view_metrics.remove(&dialog_id) else {
            return;
        };
        let mut metrics: HashMap<MessageId, MessageViewMetrics> = HashMap::new();
        for input_metric in &input_metrics_raw {
            assert!(input_metric.message_id.is_server());
            let metric = metrics.entry(input_metric.message_id).or_default();
            metric.message_id = input_metric.message_id;
            metric.time_in_view_ms += input_metric.time_in_view_ms;
            metric.active_time_in_view_ms += input_metric.active_time_in_view_ms;
            metric.height_to_viewport_ratio_per_mille = metric
                .height_to_viewport_ratio_per_mille
                .max(input_metric.height_to_viewport_ratio_per_mille);
            metric.seen_range_ratio_per_mille = metric
                .seen_range_ratio_per_mille
                .max(input_metric.seen_range_ratio_per_mille);
        }

        let input_metrics = metrics
            .into_values()
            .map(|metric| MessageReadMetric {
                message_id: metric.message_id,
                view_id: env.secure_random_i64(),
                time_in_view_ms: metric.time_in_view_ms,
                active_time_in_view_ms: metric.active_time_in_view_ms,
                height_to_viewport_ratio_per_mille: metric.height_to_viewport_ratio_per_mille,
                seen_range_ratio_per_mille: metric.seen_range_ratio_per_mille,
            })
            .collect();
        env.send_report_read_metrics_query(dialog_id, input_metrics);
    }

    /// `get_message_viewers` (MessageQueryManager.cpp:3731-3746) — the
    /// access check rides the message plane (`can_get_message_viewers`,
    /// [blocked: M.10 integration]) and the query send is
    /// `[blocked: net]`; the port carries the promise shape.
    pub fn get_message_viewers(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        message_full_id: MessageFullId,
        mut promise: Promise<td_api::MessageViewers>,
    ) {
        if let Err(error) = env.can_get_message_viewers(message_full_id) {
            return promise.set_error(error);
        }
        env.send_get_message_read_participants_query(message_full_id, promise);
    }

    /// `on_get_message_viewers` (MessageQueryManager.cpp:3748-3782) — the
    /// participant-list gate and the final object build; the full reload
    /// is the chat-manager plane ([blocked: M.6 integration]).
    pub fn on_get_message_viewers(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        message_viewers: MessageViewers,
        is_recursive: bool,
        mut promise: Promise<td_api::MessageViewers>,
    ) {
        if !is_recursive {
            let need_participant_list = message_viewers
                .get_user_ids()
                .iter()
                .any(|user_id| !env.have_user_force(*user_id, "on_get_message_viewers"));
            if need_participant_list {
                // `reload_chat_full`/`get_channel_participants` then the
                // recursive call — the [blocked: M.6 integration] seam; the
                // collapsed line answers without the participant load
                // (the C++ recursive pass only fills the user cache)
                let mut object = message_viewers.get_message_viewers_object(env);
                let _ = &mut object;
                return promise.set_value(object);
            }
        }
        promise.set_value(message_viewers.get_message_viewers_object(env));
    }

    /// `view_messages` (MessageQueryManager.cpp:3784-3809) — the dedup
    /// sets and the 100-id chunking; each chunk send is `[blocked: net]`.
    pub fn view_messages(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        dialog_id: DialogId,
        message_ids: &[MessageId],
        increment_view_counter: bool,
    ) {
        const MAX_MESSAGE_VIEWS: usize = 100; // server-side limit
        let mut viewed_message_ids = Vec::with_capacity(message_ids.len().min(MAX_MESSAGE_VIEWS));
        for message_id in message_ids {
            let message_full_id = MessageFullId::new(dialog_id, *message_id);
            if !self
                .being_reloaded_views_message_full_ids
                .insert(message_full_id)
            {
                if !increment_view_counter
                    || !self
                        .need_view_counter_increment_message_full_ids
                        .insert(message_full_id)
                {
                    continue;
                }
            } else if increment_view_counter {
                self.need_view_counter_increment_message_full_ids
                    .insert(message_full_id);
            }
            viewed_message_ids.push(*message_id);
            if viewed_message_ids.len() >= MAX_MESSAGE_VIEWS {
                env.send_get_messages_views_query(
                    dialog_id,
                    std::mem::take(&mut viewed_message_ids),
                    increment_view_counter,
                );
            }
        }
        if !viewed_message_ids.is_empty() {
            env.send_get_messages_views_query(
                dialog_id,
                viewed_message_ids,
                increment_view_counter,
            );
        }
    }

    /// `finish_get_message_views` (MessageQueryManager.cpp:3811-3817).
    pub fn finish_get_message_views(&mut self, dialog_id: DialogId, message_ids: &[MessageId]) {
        for message_id in message_ids {
            let message_full_id = MessageFullId::new(dialog_id, *message_id);
            self.being_reloaded_views_message_full_ids
                .remove(&message_full_id);
            self.need_view_counter_increment_message_full_ids
                .remove(&message_full_id);
        }
    }

    /// `queue_message_reactions_reload(MessageFullId)`
    /// (MessageQueryManager.cpp:3819-3826).
    pub fn queue_message_reactions_reload(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        message_full_id: MessageFullId,
    ) {
        let dialog_id = message_full_id.get_dialog_id();
        assert!(dialog_id.is_valid());
        let message_id = message_full_id.get_message_id();
        assert!(message_id.is_valid());
        self.being_reloaded_reactions
            .entry(dialog_id)
            .or_default()
            .message_ids
            .insert(message_id);
        self.try_reload_message_reactions(env, dialog_id, false);
    }

    /// `queue_message_reactions_reload(DialogId, vector<MessageId>)`
    /// (MessageQueryManager.cpp:3828-3836) — the overload set splits on
    /// the argument shape.
    pub fn queue_message_reactions_reload_for_dialog(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        dialog_id: DialogId,
        message_ids: &[MessageId],
    ) {
        let message_ids_to_reload = &mut self
            .being_reloaded_reactions
            .entry(dialog_id)
            .or_default()
            .message_ids;
        for message_id in message_ids {
            assert!(message_id.is_valid());
            message_ids_to_reload.insert(*message_id);
        }
        self.try_reload_message_reactions(env, dialog_id, false);
    }

    /// `try_reload_message_reactions` (MessageQueryManager.cpp:3838-3892)
    /// — the queue drain and the request flag; the query send is
    /// `[blocked: net]`, the no-input-peer retry rides the 0.2s deadline
    /// (the SleepActor stand-in).
    pub fn try_reload_message_reactions(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        dialog_id: DialogId,
        is_finished: bool,
    ) {
        if env.close_flag() {
            return;
        }

        let Some(state) = self.being_reloaded_reactions.get_mut(&dialog_id) else {
            return;
        };
        if is_finished {
            assert!(state.is_request_sent);
            state.is_request_sent = false;

            if state.message_ids.is_empty() {
                self.being_reloaded_reactions.remove(&dialog_id);
                self.reactions_reload_retry_deadlines.remove(&dialog_id);
                return;
            }
        } else if state.is_request_sent {
            return;
        }

        assert!(!state.message_ids.is_empty());
        assert!(!state.is_request_sent);

        state.is_request_sent = true;

        const MAX_MESSAGE_IDS: usize = 100; // server-side limit
        let mut message_ids = Vec::new();
        for message_id in state.message_ids.iter() {
            if message_ids.len() >= MAX_MESSAGE_IDS {
                break;
            }
            // the disjoint-field read of the pending map (the C++ reads
            // has_message_pending_read_reactions mid-iteration)
            if !self
                .pending_read_reactions
                .contains_key(&MessageFullId::new(dialog_id, *message_id))
            {
                message_ids.push(*message_id);
            }
        }
        for message_id in &message_ids {
            state.message_ids.remove(message_id);
        }
        // the C++ iterates its FlatHashSet (bucket order); the port sorts —
        // the id order is not part of the query contract, and the sort
        // pins the tests
        message_ids.sort_unstable();

        if !env.have_input_peer(dialog_id, false, crate::user_manager::AccessRights::Read)
            || message_ids.is_empty()
        {
            self.reactions_reload_retry_deadlines
                .insert(dialog_id, env.now() + 0.2);
            return;
        }

        for message_id in &message_ids {
            assert!(message_id.is_server());
        }

        env.send_get_messages_reactions_query(dialog_id, message_ids);
    }

    /// `get_paid_message_reaction_senders` +
    /// `do_get_paid_message_reaction_senders`
    /// (MessageQueryManager.cpp:3894-3938) — the M.16 routing-audit
    /// obligation. The load half (`load_created_public_broadcasts` +
    /// `get_me`) is `[blocked: net]`; the answer half needs the
    /// created-broadcasts/participant-count planes
    /// ([blocked: M.5 integration]) — the arm stays interim.
    pub fn get_paid_message_reaction_senders(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        _dialog_id: DialogId,
        promise: Promise<td_api::MessageSenders>,
    ) {
        // the C++ dialog_id rides the load; the seam drops it with the
        // promise ([blocked: net])
        env.load_paid_message_reaction_sender_state(promise);
    }

    /// `has_message_pending_read_reactions`
    /// (MessageQueryManager.cpp:5046-5048).
    pub fn has_message_pending_read_reactions(&self, message_full_id: MessageFullId) -> bool {
        self.pending_read_reactions.contains_key(&message_full_id)
    }

    /// `has_message_pending_read_poll_votes`
    /// (MessageQueryManager.cpp:5082-5084).
    pub fn has_message_pending_read_poll_votes(&self, message_full_id: MessageFullId) -> bool {
        self.pending_read_poll_votes.contains_key(&message_full_id)
    }

    /// `read_message_reactions_on_server`
    /// (MessageQueryManager.cpp:5050-5060) — the pending counter bump;
    /// the contents read is the `[blocked: net]` tail.
    pub fn read_message_reactions_on_server(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        dialog_id: DialogId,
        message_ids: Vec<MessageId>,
    ) {
        for message_id in &message_ids {
            *self
                .pending_read_reactions
                .entry(MessageFullId::new(dialog_id, *message_id))
                .or_default() += 1;
        }
        env.send_read_message_contents_query(dialog_id, message_ids);
    }

    /// `on_read_message_reactions` (MessageQueryManager.cpp:5062-5080).
    pub fn on_read_message_reactions(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        dialog_id: DialogId,
        message_ids: Vec<MessageId>,
        is_error: bool,
    ) {
        for message_id in message_ids {
            let message_full_id = MessageFullId::new(dialog_id, message_id);
            let counter = self
                .pending_read_reactions
                .get_mut(&message_full_id)
                .expect("the pending read-reactions counter underflowed");
            *counter -= 1;
            if *counter == 0 {
                self.pending_read_reactions.remove(&message_full_id);
            }

            if !env.have_message_force(message_full_id, "on_read_message_reactions") {
                continue;
            }

            if is_error {
                self.queue_message_reactions_reload(env, message_full_id);
            }
        }
    }

    /// `read_message_poll_votes_on_server`
    /// (MessageQueryManager.cpp:5086-5097).
    pub fn read_message_poll_votes_on_server(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        dialog_id: DialogId,
        message_ids: Vec<MessageId>,
    ) {
        assert!(dialog_id.is_valid());
        for message_id in &message_ids {
            *self
                .pending_read_poll_votes
                .entry(MessageFullId::new(dialog_id, *message_id))
                .or_default() += 1;
        }
        env.send_read_message_contents_query(dialog_id, message_ids);
    }

    /// `on_read_message_poll_votes` (MessageQueryManager.cpp:5099-5121).
    pub fn on_read_message_poll_votes(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        dialog_id: DialogId,
        message_ids: Vec<MessageId>,
        is_error: bool,
    ) {
        let mut message_full_ids = Vec::new();
        for message_id in message_ids {
            let message_full_id = MessageFullId::new(dialog_id, message_id);
            let counter = self
                .pending_read_poll_votes
                .get_mut(&message_full_id)
                .expect("the pending read-poll-votes counter underflowed");
            *counter -= 1;
            if *counter == 0 {
                self.pending_read_poll_votes.remove(&message_full_id);
            }

            if !env.have_message_force(message_full_id, "on_read_message_poll_votes") {
                continue;
            }
            if is_error {
                message_full_ids.push(message_full_id);
            }
        }
        if !message_full_ids.is_empty() {
            // `get_messages_from_server` — the [blocked: M.10 integration]
            // refetch
            env.get_messages_from_server(message_full_ids);
        }
    }

    /// `get_emoji_game_info` (MessageQueryManager.cpp:4059-4069) — the
    /// fetch is `[blocked: net]`; the promise lands in the query.
    pub fn get_emoji_game_info(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        promise: Promise<td_api::StakeDiceState>,
    ) {
        env.send_get_emoji_game_info_query(promise);
    }

    /// `on_get_emoji_game_info` (MessageQueryManager.cpp:4071-4077) —
    /// fully local once the wire answer is in.
    pub fn on_get_emoji_game_info(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        game_info: &telegram_api::messages_EmojiGameInfo,
        mut promise: Promise<td_api::StakeDiceState>,
    ) {
        self.on_update_emoji_game_info(env, game_info);
        assert!(self.is_emoji_game_info_inited);
        promise.set_value(self.emoji_game_info.get_stake_dice_state_object(env));
    }

    /// `on_update_emoji_game_info` (MessageQueryManager.cpp:5158-5171) —
    /// the bot gate, the equality no-op and the update send.
    pub fn on_update_emoji_game_info(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        game_info: &telegram_api::messages_EmojiGameInfo,
    ) {
        let emoji_game_info = EmojiGameInfo::from_wire(game_info);
        if env.auth_is_bot() {
            return;
        }
        if self.is_emoji_game_info_inited && emoji_game_info == self.emoji_game_info {
            return;
        }
        self.emoji_game_info = emoji_game_info;
        self.emoji_game_info_receive_time = env.now();
        self.is_emoji_game_info_inited = true;
        let update = self.emoji_game_info.get_update_stake_dice_state_object(env);
        env.send_update(update);
    }

    /// `get_current_state` (MessageQueryManager.cpp:5173-5181) — the
    /// 60-second freshness window.
    pub fn get_current_state(
        &self,
        env: &impl MessageQueryManagerEnv,
        updates: &mut Vec<td_api::Update>,
    ) {
        if env.auth_is_bot() {
            return;
        }

        if self.is_emoji_game_info_inited && self.emoji_game_info_receive_time > env.now() - 60.0 {
            updates.push(self.emoji_game_info.get_update_stake_dice_state_object(env));
        }
    }

    /// `send_message_view_metrics_timeout_` + the SleepActor retry, folded
    /// into the collapsed-line deadline tick.
    pub fn tick_timeouts(&mut self, env: &mut impl MessageQueryManagerEnv) {
        let now = env.now();
        let fired_metrics: Vec<DialogId> = self
            .view_metrics_deadlines
            .iter()
            .filter(|(_, deadline)| **deadline <= now)
            .map(|(dialog_id, _)| *dialog_id)
            .collect();
        for dialog_id in fired_metrics {
            self.view_metrics_deadlines.remove(&dialog_id);
            self.send_message_view_metrics_timeout(env, dialog_id);
        }

        let fired_retries: Vec<DialogId> = self
            .reactions_reload_retry_deadlines
            .iter()
            .filter(|(_, deadline)| **deadline <= now)
            .map(|(dialog_id, _)| *dialog_id)
            .collect();
        for dialog_id in fired_retries {
            self.reactions_reload_retry_deadlines.remove(&dialog_id);
            self.try_reload_message_reactions(env, dialog_id, true);
        }
    }

    /// The merged next deadline of the two timeout families.
    pub fn next_deadline(&self) -> Option<f64> {
        self.view_metrics_deadlines
            .values()
            .chain(self.reactions_reload_retry_deadlines.values())
            .fold(None, |acc: Option<f64>, deadline| {
                Some(acc.map_or(*deadline, |current| current.min(*deadline)))
            })
    }
}

impl std::fmt::Debug for MessageQueryManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MessageQueryManager")
            .field("pending_read_reactions", &self.pending_read_reactions.len())
            .field(
                "pending_read_poll_votes",
                &self.pending_read_poll_votes.len(),
            )
            .field(
                "being_reloaded_reactions",
                &self.being_reloaded_reactions.len(),
            )
            .field(
                "pending_message_view_metrics",
                &self.pending_message_view_metrics.len(),
            )
            .field("is_emoji_game_info_inited", &self.is_emoji_game_info_inited)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// The M.mp request plane — `search_messages` and friends
// (MessageQueryManager.cpp:3206-4298/4715-4835). The local validation and
// decision tables are ported exactly; every `create_handler` query tail is
// the `[blocked: net]` seam above and the promise lands unanswered offline.
// ---------------------------------------------------------------------------

/// `get_message_search_filter` (MessageSearchFilter.cpp:59-103) — the
/// td_api filter table; `None` maps to `Empty`.
pub fn get_message_search_filter(
    filter: Option<&td_api::SearchMessagesFilter>,
) -> crate::messages_manager::MessageSearchFilter {
    use crate::messages_manager::MessageSearchFilter as F;
    let Some(filter) = filter else {
        return F::Empty;
    };
    match filter {
        td_api::SearchMessagesFilter::searchMessagesFilterEmpty(_) => F::Empty,
        td_api::SearchMessagesFilter::searchMessagesFilterAnimation(_) => F::Animation,
        td_api::SearchMessagesFilter::searchMessagesFilterAudio(_) => F::Audio,
        td_api::SearchMessagesFilter::searchMessagesFilterDocument(_) => F::Document,
        td_api::SearchMessagesFilter::searchMessagesFilterPhoto(_) => F::Photo,
        td_api::SearchMessagesFilter::searchMessagesFilterVideo(_) => F::Video,
        td_api::SearchMessagesFilter::searchMessagesFilterVoiceNote(_) => F::VoiceNote,
        td_api::SearchMessagesFilter::searchMessagesFilterPhotoAndVideo(_) => F::PhotoAndVideo,
        td_api::SearchMessagesFilter::searchMessagesFilterUrl(_) => F::Url,
        td_api::SearchMessagesFilter::searchMessagesFilterChatPhoto(_) => F::ChatPhoto,
        td_api::SearchMessagesFilter::searchMessagesFilterVideoNote(_) => F::VideoNote,
        td_api::SearchMessagesFilter::searchMessagesFilterVoiceAndVideoNote(_) => {
            F::VoiceAndVideoNote
        }
        td_api::SearchMessagesFilter::searchMessagesFilterMention(_) => F::Mention,
        td_api::SearchMessagesFilter::searchMessagesFilterUnreadMention(_) => F::UnreadMention,
        td_api::SearchMessagesFilter::searchMessagesFilterFailedToSend(_) => F::FailedToSend,
        td_api::SearchMessagesFilter::searchMessagesFilterPinned(_) => F::Pinned,
        td_api::SearchMessagesFilter::searchMessagesFilterUnreadReaction(_) => F::UnreadReaction,
        td_api::SearchMessagesFilter::searchMessagesFilterPoll(_) => F::Poll,
        td_api::SearchMessagesFilter::searchMessagesFilterUnreadPollVote(_) => F::UnreadPollVote,
        #[allow(unreachable_patterns)]
        _ => {
            tracing::error!("Receive unsupported search messages filter");
            F::Empty
        }
    }
}

/// `MessagesManager::get_found_messages_object` over the empty
/// `FoundMessages` — the local empty answer of the two search arms.
fn get_empty_found_messages() -> td_api::FoundMessages {
    td_api::FoundMessages {
        total_count: 0,
        messages: Vec::new(),
        next_offset: String::new(),
    }
}

/// The search-limit clamps shared by the search arms — `Ok(None)` on the
/// 400 band, `Ok(Some(clamped))` otherwise
/// (`limit <= 0` / `limit > MAX_SEARCH_MESSAGES`).
fn fix_search_limit(mut limit: i32) -> Result<i32, tdutils::status::Status> {
    if limit <= 0 {
        return Err(tdutils::status::Status::error(
            400,
            "Parameter limit must be positive",
        ));
    }
    if limit > MAX_SEARCH_MESSAGES {
        limit = MAX_SEARCH_MESSAGES;
    }
    Ok(limit)
}

impl MessageQueryManager {
    /// `search_messages` (MessageQueryManager.cpp:3333-3370).
    #[allow(clippy::too_many_arguments)]
    pub fn search_messages(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        dialog_list_id: crate::messages_manager::DialogListId,
        ignore_folder_id: bool,
        query: String,
        offset_str: String,
        limit: i32,
        filter: crate::messages_manager::MessageSearchFilter,
        dialog_type_filter: Option<td_api::SearchMessagesChatTypeFilter>,
        min_date: i32,
        max_date: i32,
        mut promise: Promise<td_api::FoundMessages>,
    ) {
        if !dialog_list_id.is_folder() {
            promise.set_error(tdutils::status::Status::error(
                400,
                "Wrong chat list specified",
            ));
            return;
        }
        let limit = match fix_search_limit(limit) {
            Ok(limit) => limit,
            Err(error) => {
                promise.set_error(error);
                return;
            }
        };
        let offset = match crate::message_query_manager::MessageSearchOffset::from_offset_string(
            &offset_str,
        ) {
            Ok(offset) => offset,
            Err(error) => {
                promise.set_error(error);
                return;
            }
        };

        // the C++ CHECK that the arm's `get_message_search_filter` mapping
        // never routes the Call filters here
        assert!(filter != crate::messages_manager::MessageSearchFilter::Call);
        assert!(filter != crate::messages_manager::MessageSearchFilter::MissedCall);
        if matches!(
            filter,
            crate::messages_manager::MessageSearchFilter::Mention
                | crate::messages_manager::MessageSearchFilter::UnreadMention
                | crate::messages_manager::MessageSearchFilter::UnreadReaction
                | crate::messages_manager::MessageSearchFilter::UnreadPollVote
                | crate::messages_manager::MessageSearchFilter::FailedToSend
                | crate::messages_manager::MessageSearchFilter::Pinned
        ) {
            promise.set_error(tdutils::status::Status::error(
                400,
                "The filter is not supported",
            ));
            return;
        }

        if query.is_empty() && filter == crate::messages_manager::MessageSearchFilter::Empty {
            promise.set_value(get_empty_found_messages());
            return;
        }

        env.send_search_messages_global_query(
            dialog_list_id.get_folder_id(),
            ignore_folder_id,
            query,
            offset.date,
            offset.dialog_id,
            offset.message_id,
            limit,
            filter,
            dialog_type_filter,
            min_date,
            max_date,
            promise,
        );
    }

    /// `search_outgoing_document_messages`
    /// (MessageQueryManager.cpp:3404-3414).
    pub fn search_outgoing_document_messages(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        query: String,
        limit: i32,
        mut promise: Promise<td_api::FoundMessages>,
    ) {
        let limit = match fix_search_limit(limit) {
            Ok(limit) => limit,
            Err(error) => {
                promise.set_error(error);
                return;
            }
        };
        env.send_search_sent_media_query(query, limit, promise);
    }

    /// `check_search_posts_flood` (MessageQueryManager.cpp:3437-3441).
    pub fn check_search_posts_flood(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        query: String,
        promise: Promise<td_api::PublicPostSearchLimits>,
    ) {
        env.send_check_search_posts_flood_query(query, promise);
    }

    /// `search_public_posts` (MessageQueryManager.cpp:3442-3468).
    pub fn search_public_posts(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        query: String,
        offset_str: String,
        limit: i32,
        star_count: i64,
        mut promise: Promise<td_api::FoundPublicPosts>,
    ) {
        let limit = match fix_search_limit(limit) {
            Ok(limit) => limit,
            Err(error) => {
                promise.set_error(error);
                return;
            }
        };
        if star_count < 0 {
            promise.set_error(tdutils::status::Status::error(
                400,
                "Invalid number of Telegram Stars specified",
            ));
            return;
        }
        if !env.has_owned_star_count(star_count) {
            promise.set_error(tdutils::status::Status::error(400, "BALANCE_TOO_LOW"));
            return;
        }
        let offset = match crate::message_query_manager::MessageSearchOffset::from_offset_string(
            &offset_str,
        ) {
            Ok(offset) => offset,
            Err(error) => {
                promise.set_error(error);
                return;
            }
        };
        env.send_search_public_posts_query(query, offset, limit, star_count, promise);
    }

    /// `search_hashtag_posts` (MessageQueryManager.cpp:3496-3535).
    pub fn search_hashtag_posts(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        mut hashtag: String,
        offset_str: String,
        limit: i32,
        mut promise: Promise<td_api::FoundMessages>,
    ) {
        let limit = match fix_search_limit(limit) {
            Ok(limit) => limit,
            Err(error) => {
                promise.set_error(error);
                return;
            }
        };
        let offset = match crate::message_query_manager::MessageSearchOffset::from_offset_string(
            &offset_str,
        ) {
            Ok(offset) => offset,
            Err(error) => {
                promise.set_error(error);
                return;
            }
        };

        let mut is_cashtag = false;
        let first_byte = hashtag.as_bytes().first().copied();
        if matches!(first_byte, Some(b'#') | Some(b'$')) {
            is_cashtag = first_byte == Some(b'$');
            hashtag.remove(0);
        }
        if hashtag.is_empty() {
            promise.set_value(get_empty_found_messages());
            return;
        }
        env.hashtag_used(is_cashtag, hashtag.clone());

        let mut prefixed = if is_cashtag { "$" } else { "#" }.to_string();
        prefixed.push_str(&hashtag);
        env.send_search_posts_query(prefixed, offset, limit, promise);
    }

    /// `search_dialog_recent_location_messages`
    /// (MessageQueryManager.cpp:3557-3582).
    pub fn search_dialog_recent_location_messages(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        dialog_id: DialogId,
        limit: i32,
        mut promise: Promise<td_api::Messages>,
    ) {
        tracing::info!(
            "Search recent location messages in {} with limit {}",
            dialog_id,
            limit
        );
        let limit = match fix_search_limit(limit) {
            Ok(limit) => limit,
            Err(error) => {
                promise.set_error(error);
                return;
            }
        };
        if let Err(error) = env.check_dialog_access(
            dialog_id,
            true,
            crate::user_manager::AccessRights::Read,
            "search_dialog_recent_location_messages",
        ) {
            promise.set_error(error);
            return;
        }

        use crate::dialog_id::DialogType;
        match dialog_id.get_type() {
            DialogType::User | DialogType::Chat | DialogType::Channel => {
                env.send_get_recent_locations_query(dialog_id, limit, promise);
            }
            DialogType::SecretChat => promise.set_value(
                crate::messages_manager::get_messages_object(0, Vec::new(), false),
            ),
            _ => {
                // the C++ UNREACHABLE behind the check_dialog_access gate
                promise.set_error(tdutils::status::Status::error(
                    500,
                    "Message search is not supported",
                ));
            }
        }
    }

    /// `report_music_listen` (MessageQueryManager.cpp:3630-3652).
    pub fn report_music_listen(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        file_id: crate::files::FileId,
        duration: i32,
        mut promise: Promise<()>,
    ) {
        if duration < 0 {
            promise.set_error(tdutils::status::Status::error(
                400,
                "Invalid duration specified",
            ));
            return;
        }
        if !file_id.is_valid() {
            promise.set_error(tdutils::status::Status::error(
                400,
                "Invalid file identifier specified",
            ));
            return;
        }
        let Some(input_document) = env.get_music_listen_input_document(file_id) else {
            // encrypted, non-document, web or missing remote location
            promise.set_value(());
            return;
        };
        if duration < 3 {
            promise.set_value(());
            return;
        }
        env.send_report_music_listen_query(input_document, duration, promise);
    }

    /// `summarize_message_text` (MessageQueryManager.cpp:3940-3955).
    pub fn summarize_message_text(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        message_full_id: MessageFullId,
        to_language_code: String,
        mut tone: String,
        mut promise: Promise<td_api::FormattedText>,
    ) {
        let dialog_id = message_full_id.get_dialog_id();
        if let Err(error) = env.check_dialog_access(
            dialog_id,
            false,
            crate::user_manager::AccessRights::Read,
            "summarize_message_text",
        ) {
            promise.set_error(error);
            return;
        }
        if !env.have_message_force(message_full_id, "summarize_message_text") {
            promise.set_error(tdutils::status::Status::error(400, "Message not found"));
            return;
        }
        if let tdutils::status::Result::Err(error) =
            crate::translation_manager::TranslationManager::check_tone(&mut tone)
        {
            promise.set_error(error);
            return;
        }
        let message_id = message_full_id.get_message_id();
        if !message_id.is_server() {
            promise.set_error(tdutils::status::Status::error(
                400,
                "Message can't be summarized",
            ));
            return;
        }
        env.send_summarize_text_query(dialog_id, message_id, to_language_code, tone, promise);
    }

    /// `add_to_do_list_tasks` (MessageQueryManager.cpp:3957-3974).
    pub fn add_to_do_list_tasks(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        message_full_id: MessageFullId,
        tasks: Vec<td_api::InputChecklistTask>,
        mut promise: Promise<()>,
    ) {
        let dialog_id = message_full_id.get_dialog_id();
        if !env.can_add_message_tasks(message_full_id, tasks.len().try_into().unwrap_or(i32::MAX)) {
            promise.set_error(tdutils::status::Status::error(
                400,
                "Can't add the checklist tasks to the message",
            ));
            return;
        }
        let mut items = Vec::with_capacity(tasks.len());
        for task in tasks {
            match env.parse_to_do_item(dialog_id, task) {
                Ok(item) => items.push(item),
                Err(error) => {
                    promise.set_error(error);
                    return;
                }
            }
        }
        if items.is_empty() {
            promise.set_error(tdutils::status::Status::error(
                400,
                "The list of new tasks must be non-empty",
            ));
            return;
        }
        env.send_append_to_do_list_query(
            dialog_id,
            message_full_id.get_message_id(),
            items,
            promise,
        );
    }

    /// `mark_to_do_list_tasks_as_done` (MessageQueryManager.cpp:3976-3985).
    pub fn mark_to_do_list_tasks_as_done(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        message_full_id: MessageFullId,
        done_task_ids: Vec<i32>,
        not_done_task_ids: Vec<i32>,
        mut promise: Promise<()>,
    ) {
        let dialog_id = message_full_id.get_dialog_id();
        if !env.can_mark_message_tasks_as_done(message_full_id) {
            promise.set_error(tdutils::status::Status::error(
                400,
                "Can't mark tasks as done in the message",
            ));
            return;
        }
        env.send_toggle_to_do_completed_query(
            dialog_id,
            message_full_id.get_message_id(),
            done_task_ids,
            not_done_task_ids,
            promise,
        );
    }

    /// `edit_ephemeral_message` (MessageQueryManager.cpp:4131-4179).
    #[allow(clippy::too_many_arguments)]
    pub fn edit_ephemeral_message(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        dialog_id: DialogId,
        receiver_user_id: UserId,
        ephemeral_message_id: EphemeralMessageId,
        reply_markup: Option<td_api::ReplyMarkup>,
        input_message_content: Option<td_api::InputMessageContent>,
        mut promise: Promise<()>,
    ) {
        let is_bot = env.auth_is_bot();
        assert!(is_bot);

        if let Err(error) = env.check_dialog_access(
            dialog_id,
            false,
            crate::user_manager::AccessRights::Write,
            "edit_ephemeral_message",
        ) {
            promise.set_error(error);
            return;
        }
        let new_reply_markup = match crate::reply_markup::get_inline_reply_markup(
            reply_markup,
            is_bot,
            true,
            env.is_test_dc(),
        ) {
            Ok(new_reply_markup) => new_reply_markup,
            Err(error) => {
                promise.set_error(error);
                return;
            }
        };
        let Some(input_message_content) = input_message_content else {
            env.send_edit_ephemeral_message_query(
                dialog_id,
                receiver_user_id,
                ephemeral_message_id,
                false,
                None,
                false,
                false,
                new_reply_markup,
                promise,
            );
            return;
        };

        if matches!(
            &input_message_content,
            td_api::InputMessageContent::inputMessageText(_)
        ) {
            let input_message_text =
                match env.parse_input_message_text(dialog_id, input_message_content) {
                    Ok(input_message_text) => input_message_text,
                    Err(error) => {
                        promise.set_error(error);
                        return;
                    }
                };
            env.send_edit_ephemeral_message_query(
                dialog_id,
                receiver_user_id,
                ephemeral_message_id,
                true,
                Some(input_message_text.text),
                input_message_text.disable_web_page_preview,
                false,
                new_reply_markup,
                promise,
            );
            return;
        }

        let content = match env.parse_input_message_content(dialog_id, input_message_content) {
            Ok(content) => content,
            Err(error) => {
                promise.set_error(error);
                return;
            }
        };
        let content_type = content.content.get_type();
        if !crate::messages_manager::is_editable_media_message_content(content_type)
            && content_type != crate::message_content::MessageContentType::RichText
        {
            promise.set_error(tdutils::status::Status::error(
                400,
                "Unsupported input message content type",
            ));
            return;
        }
        if !content.ttl.is_empty() {
            promise.set_error(tdutils::status::Status::error(
                400,
                "Can't enable self-destruction for media",
            ));
            return;
        }

        env.start_edit_ephemeral_message_upload(
            dialog_id,
            receiver_user_id,
            ephemeral_message_id,
            new_reply_markup,
            *content.content,
            content.invert_media,
            promise,
        );
    }

    /// `edit_ephemeral_message_caption`
    /// (MessageQueryManager.cpp:4181-4207).
    #[allow(clippy::too_many_arguments)]
    pub fn edit_ephemeral_message_caption(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        dialog_id: DialogId,
        receiver_user_id: UserId,
        ephemeral_message_id: EphemeralMessageId,
        reply_markup: Option<td_api::ReplyMarkup>,
        input_caption: Option<td_api::FormattedText>,
        invert_media: bool,
        mut promise: Promise<()>,
    ) {
        let is_bot = env.auth_is_bot();
        assert!(is_bot);

        if let Err(error) = env.check_dialog_access(
            dialog_id,
            false,
            crate::user_manager::AccessRights::Write,
            "edit_ephemeral_message_caption",
        ) {
            promise.set_error(error);
            return;
        }
        let new_reply_markup = match crate::reply_markup::get_inline_reply_markup(
            reply_markup,
            is_bot,
            true,
            env.is_test_dc(),
        ) {
            Ok(new_reply_markup) => new_reply_markup,
            Err(error) => {
                promise.set_error(error);
                return;
            }
        };
        let caption = match env.parse_input_caption(dialog_id, input_caption) {
            Ok(caption) => caption,
            Err(error) => {
                promise.set_error(error);
                return;
            }
        };

        env.send_edit_ephemeral_message_query(
            dialog_id,
            receiver_user_id,
            ephemeral_message_id,
            true,
            Some(caption),
            false,
            invert_media,
            new_reply_markup,
            promise,
        );
    }

    /// `delete_dialog_messages_by_sender`
    /// (MessageQueryManager.cpp:4251-4265).
    pub fn delete_dialog_messages_by_sender(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        dialog_id: DialogId,
        sender_dialog_id: DialogId,
        mut promise: Promise<()>,
    ) {
        assert!(!env.auth_is_bot());
        if let Err(error) = env.check_dialog_access(
            dialog_id,
            true,
            crate::user_manager::AccessRights::Write,
            "delete_dialog_messages_by_sender",
        ) {
            promise.set_error(error);
            return;
        }
        if let Err(error) = env.can_delete_all_dialog_messages_by_sender(dialog_id) {
            promise.set_error(error);
            return;
        }
        if !env.have_input_peer(
            sender_dialog_id,
            false,
            crate::user_manager::AccessRights::Know,
        ) {
            promise.set_error(tdutils::status::Status::error(
                400,
                "Message sender not found",
            ));
            return;
        }

        env.delete_local_dialog_messages_by_sender(dialog_id, sender_dialog_id);

        assert!(dialog_id.get_type() == crate::dialog_id::DialogType::Channel);
        env.send_affected_history_query(
            dialog_id,
            AffectedHistoryQueryKind::DeleteAllChannelMessagesFromSender { sender_dialog_id },
            true,
            promise,
        );
    }

    /// `delete_dialog_messages_by_date`
    /// (MessageQueryManager.cpp:4267-4297).
    pub fn delete_dialog_messages_by_date(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        dialog_id: DialogId,
        min_date: i32,
        max_date: i32,
        revoke: bool,
        mut promise: Promise<()>,
    ) {
        assert!(!env.auth_is_bot());
        if let Err(error) = env.check_dialog_access(
            dialog_id,
            false,
            crate::user_manager::AccessRights::Read,
            "delete_dialog_messages_by_date",
        ) {
            promise.set_error(error);
            return;
        }
        let (min_date, max_date) =
            match Self::fix_delete_message_min_max_dates(env, min_date, max_date) {
                Ok(fixed) => fixed,
                Err(error) => {
                    promise.set_error(error);
                    return;
                }
            };
        if max_date == 0 {
            promise.set_value(());
            return;
        }

        use crate::dialog_id::DialogType;
        match dialog_id.get_type() {
            DialogType::User => {}
            DialogType::Chat => {
                if revoke {
                    promise.set_error(tdutils::status::Status::error(
                        400,
                        "Bulk message revocation is unsupported in basic group chats",
                    ));
                    return;
                }
            }
            DialogType::Channel => {
                promise.set_error(tdutils::status::Status::error(
                    400,
                    "Bulk message deletion is unsupported in supergroup chats",
                ));
                return;
            }
            _ => {
                // the C++ UNREACHABLE — SecretChat/None are gated upstream
                unreachable!("the C++ UNREACHABLE over the dialog type fired");
            }
        }

        env.delete_local_dialog_messages_by_date(dialog_id, min_date, max_date);

        env.send_affected_history_query(
            dialog_id,
            AffectedHistoryQueryKind::DeleteDialogMessagesByDate {
                min_date,
                max_date,
                revoke,
            },
            true,
            promise,
        );
    }

    /// `delete_all_call_messages` (MessageQueryManager.cpp:4299-4302) —
    /// straight into the affected-history loop.
    pub fn delete_all_call_messages(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        revoke: bool,
        promise: Promise<()>,
    ) {
        self.run_affected_history_query_until_complete(
            env,
            DialogId::default(),
            AffectedHistoryQueryKind::DeleteAllCallMessages { revoke },
            true,
            promise,
        );
    }

    /// `delete_ephemeral_message_on_server`
    /// (MessageQueryManager.cpp:4715-4736).
    pub fn delete_ephemeral_message_on_server(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        dialog_id: DialogId,
        receiver_dialog_id: DialogId,
        ephemeral_message_id: EphemeralMessageId,
        promise: Promise<()>,
    ) {
        tracing::info!(
            "Delete {:?} from {} in {} from server",
            ephemeral_message_id,
            receiver_dialog_id,
            dialog_id
        );

        env.on_delete_ephemeral_messages(dialog_id, vec![ephemeral_message_id]);

        env.send_delete_ephemeral_message_query(
            dialog_id,
            receiver_dialog_id,
            ephemeral_message_id,
            promise,
        );
    }

    /// `delete_reactions_by_sender` (MessageQueryManager.cpp:4790-4801).
    pub fn delete_reactions_by_sender(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        dialog_id: DialogId,
        sender_dialog_id: DialogId,
        mut promise: Promise<()>,
    ) {
        if let Err(error) = env.check_dialog_access(
            dialog_id,
            true,
            crate::user_manager::AccessRights::Write,
            "delete_reactions_by_sender",
        ) {
            promise.set_error(error);
            return;
        }
        if !env.have_input_peer(
            sender_dialog_id,
            false,
            crate::user_manager::AccessRights::Know,
        ) {
            promise.set_error(tdutils::status::Status::error(
                400,
                "Reaction sender not found",
            ));
            return;
        }

        env.send_delete_participant_reactions_query(dialog_id, sender_dialog_id, promise);
    }

    /// `delete_reaction_by_sender` (MessageQueryManager.cpp:4803-4818).
    pub fn delete_reaction_by_sender(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        dialog_id: DialogId,
        message_id: MessageId,
        sender_dialog_id: DialogId,
        mut promise: Promise<()>,
    ) {
        if let Err(error) = env.check_dialog_access(
            dialog_id,
            true,
            crate::user_manager::AccessRights::Write,
            "delete_reactions_by_sender",
        ) {
            promise.set_error(error);
            return;
        }
        if !env.have_input_peer(
            sender_dialog_id,
            false,
            crate::user_manager::AccessRights::Know,
        ) {
            promise.set_error(tdutils::status::Status::error(
                400,
                "Reaction sender not found",
            ));
            return;
        }
        if !message_id.is_server() {
            promise.set_error(tdutils::status::Status::error(
                400,
                "Invalid message identifier specified",
            ));
            return;
        }

        env.send_delete_participant_reaction_query(
            dialog_id,
            message_id,
            sender_dialog_id,
            promise,
        );
    }

    /// `get_personal_chat_history` (MessageQueryManager.cpp:4820-4829).
    pub fn get_personal_chat_history(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        user_id: UserId,
        limit: i32,
        mut promise: Promise<td_api::Messages>,
    ) {
        let input_user = match env.get_input_user(user_id) {
            Ok(input_user) => input_user,
            Err(error) => {
                promise.set_error(error);
                return;
            }
        };
        if limit <= 0 {
            promise.set_error(tdutils::status::Status::error(
                400,
                "Limit must be positive",
            ));
            return;
        }

        env.send_get_personal_channel_history_query(input_user, limit, promise);
    }

    /// `share_dialogs_with_bot` (MessageQueryManager.cpp:3206-3271).
    #[allow(clippy::too_many_arguments)]
    pub fn share_dialogs_with_bot(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        source: Option<td_api::KeyboardButtonSource>,
        button_id: i32,
        shared_dialog_ids: Vec<DialogId>,
        expect_user: bool,
        only_check: bool,
        mut promise: Promise<()>,
    ) {
        let Some(source) = source else {
            promise.set_error(tdutils::status::Status::error(
                400,
                "Source must be non-empty",
            ));
            return;
        };
        let mut message_full_id = MessageFullId::default();
        let mut bot_user_id = UserId::default();
        let mut request_id = String::new();
        let requested_dialog_type = match source {
            td_api::KeyboardButtonSource::keyboardButtonSourceMessage(source) => {
                message_full_id = MessageFullId::new(
                    DialogId::new(source.chat_id),
                    MessageId::new(source.message_id),
                );
                match env.get_message_requested_dialog_type(message_full_id, button_id) {
                    Ok(requested_dialog_type) => requested_dialog_type,
                    Err(error) => {
                        promise.set_error(error);
                        return;
                    }
                }
            }
            td_api::KeyboardButtonSource::keyboardButtonSourceWebApp(source) => {
                bot_user_id = UserId::new(source.bot_user_id);
                request_id = source.prepared_button_id;
                match env.get_inline_requested_dialog_type(bot_user_id, &request_id) {
                    Some(requested_dialog_type) => requested_dialog_type,
                    None => {
                        promise.set_error(tdutils::status::Status::error(400, "Button not found"));
                        return;
                    }
                }
            }
            #[allow(unreachable_patterns)]
            _ => {
                // the C++ UNREACHABLE over the third source kind
                unreachable!("the C++ UNREACHABLE over the keyboard button source fired");
            }
        };
        if let Err(error) = requested_dialog_type.check_shared_dialog_count(shared_dialog_ids.len())
        {
            promise.set_error(error);
            return;
        }

        for &shared_dialog_id in &shared_dialog_ids {
            if shared_dialog_id.get_type() != crate::dialog_id::DialogType::User {
                if !env.have_dialog_force(shared_dialog_id, "share_dialogs_with_bot") {
                    promise.set_error(tdutils::status::Status::error(400, "Shared chat not found"));
                    return;
                }
            } else {
                if !expect_user {
                    promise.set_error(tdutils::status::Status::error(400, "Wrong chat type"));
                    return;
                }
                if !env.have_accessible_user(shared_dialog_id.get_user_id()) {
                    promise.set_error(tdutils::status::Status::error(400, "Shared user not found"));
                    return;
                }
            }
            if let Err(error) = requested_dialog_type
                .check_shared_dialog(&ShareDialogsContext { env }, shared_dialog_id)
            {
                promise.set_error(error);
                return;
            }
        }
        if only_check {
            promise.set_value(());
            return;
        }

        env.send_send_bot_requested_peer_query(
            message_full_id,
            bot_user_id,
            request_id,
            button_id,
            shared_dialog_ids,
            promise,
        );
    }
}

/// The `RequestedDialogTypeEnv` bridge for `check_shared_dialog` — the
/// probes delegate to the env (the collapsed conservative bands; the
/// `Td` impl reads the real UserManager/ChatManager).
struct ShareDialogsContext<'a, E: MessageQueryManagerEnv + ?Sized> {
    env: &'a mut E,
}

impl<E: MessageQueryManagerEnv + ?Sized> crate::requested_dialog_type::RequestedDialogTypeEnv
    for ShareDialogsContext<'_, E>
{
    fn is_user_bot(&self, user_id: UserId) -> bool {
        self.env.is_user_bot(user_id)
    }
    fn is_user_premium(&self, user_id: UserId) -> bool {
        self.env.is_user_premium(user_id)
    }
    fn get_chat_is_active(&self, chat_id: crate::chat_manager::ChatId) -> bool {
        self.env.get_chat_is_active(chat_id)
    }
    fn get_chat_status(
        &self,
        chat_id: crate::chat_manager::ChatId,
    ) -> crate::chat_manager::DialogParticipantStatus {
        self.env.get_chat_status(chat_id)
    }
    fn is_broadcast_channel(&self, channel_id: ChannelId) -> bool {
        self.env.is_broadcast_channel(channel_id)
    }
    fn is_monoforum_channel(&self, channel_id: ChannelId) -> bool {
        self.env.is_monoforum_channel(channel_id)
    }
    fn is_forum_channel(&self, channel_id: ChannelId) -> bool {
        self.env.is_forum_channel(channel_id)
    }
    fn get_channel_first_username(&self, channel_id: ChannelId) -> String {
        self.env.get_channel_first_username(channel_id)
    }
    fn get_channel_status(
        &self,
        channel_id: ChannelId,
    ) -> crate::chat_manager::DialogParticipantStatus {
        self.env.get_channel_status(channel_id)
    }
    fn is_channel_public(&self, channel_id: ChannelId) -> bool {
        self.env.is_channel_public(channel_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store_parse::Parser;
    use crate::store_parse::store_log_event_version;
    use crate::user_manager::UserId;
    use std::collections::{HashMap, HashSet};
    use tdmtproto::telegram_api;

    /// The offline [`MessageQueryManagerEnv`] stub — known users live in a
    /// set, options in a map.
    #[derive(Default)]
    struct TestEnv {
        known_users: HashSet<i64>,
        options: HashMap<String, String>,
        unix_time: i32,
        is_bot: bool,
    }

    impl MessageQueryManagerEnv for TestEnv {
        fn auth_is_bot(&self) -> bool {
            self.is_bot
        }

        fn close_flag(&self) -> bool {
            false
        }

        fn unix_time(&self) -> i32 {
            self.unix_time
        }

        fn get_option_string(&self, name: &str, default_value: &str) -> String {
            self.options
                .get(name)
                .cloned()
                .unwrap_or_else(|| default_value.to_owned())
        }

        fn get_user_id_object(&mut self, user_id: UserId, _source: &str) -> i64 {
            if self.known_users.contains(&user_id.get()) {
                user_id.get()
            } else {
                // the UserManager answers 0 for an unknown user
                0
            }
        }

        fn have_user_force(&mut self, user_id: UserId, _source: &str) -> bool {
            self.known_users.contains(&user_id.get())
        }

        fn now(&self) -> f64 {
            0.0
        }

        fn secure_random_i64(&self) -> i64 {
            0
        }

        fn send_update(&mut self, _update: td_api::Update) {}
    }

    fn read_participant_date(user_id: i64, date: i32) -> telegram_api::ReadParticipantDate {
        telegram_api::ReadParticipantDate { user_id, date }
    }

    fn user_dialog(user_id: i64) -> DialogId {
        DialogId::from_user_id(UserId::new(user_id))
    }

    fn chat_dialog(chat_id: i64) -> DialogId {
        DialogId::from_chat_id(crate::chat_manager::ChatId::new(chat_id))
    }

    fn sample_message(id: i32, date: i32, user_id: i64) -> telegram_api::Message {
        telegram_api::Message::message(telegram_api::message {
            id,
            date,
            peer_id: Some(telegram_api::Peer::peerUser(telegram_api::peerUser {
                user_id,
            })),
            ..Default::default()
        })
    }

    #[test]
    fn message_viewer_clamps_the_date_and_detects_emptiness() {
        let viewer = MessageViewer::new(UserId::new(42), -5);
        assert_eq!(viewer, MessageViewer::new(UserId::new(42), 0));
        assert!(!viewer.is_empty());
        assert!(MessageViewer::default().is_empty());

        let from_wire = MessageViewer::from_read_participant_date(&read_participant_date(43, 17));
        assert_eq!(from_wire, MessageViewer::new(UserId::new(43), 17));
    }

    #[test]
    fn message_viewers_intake_drops_invalid_users_and_builds_objects() {
        let mut env = TestEnv {
            known_users: HashSet::from([42]),
            ..TestEnv::default()
        };
        let viewers = MessageViewers::from_read_participant_dates(vec![
            read_participant_date(42, 100),
            read_participant_date(0, 101),  // invalid: zero
            read_participant_date(-7, 102), // invalid: negative
        ]);
        assert_eq!(viewers.get_user_ids(), vec![UserId::new(42)]);

        let object = viewers.get_message_viewers_object(&mut env);
        assert_eq!(object.viewers.len(), 1);
        assert_eq!(object.viewers[0].user_id, 42);
        assert_eq!(object.viewers[0].view_date, 100);

        // an unknown user still builds, answering the 0 id like the C++
        let unknown =
            MessageViewers::from_read_participant_dates(vec![read_participant_date(43, 5)]);
        let object = unknown.get_message_viewers_object(&mut env);
        assert_eq!(object.viewers[0].user_id, 0);
        assert_eq!(object.viewers[0].view_date, 5);
    }

    #[test]
    fn affected_history_folds_the_offset_into_is_final() {
        let history =
            AffectedHistory::from_affected_history(&telegram_api::messages_AffectedHistory {
                pts: 10,
                pts_count: 2,
                offset: 0,
            });
        assert_eq!(
            history,
            AffectedHistory {
                pts: 10,
                pts_count: 2,
                is_final: true
            }
        );

        let history = AffectedHistory::from_affected_found_messages(
            &telegram_api::messages_AffectedFoundMessages {
                pts: 11,
                pts_count: 1,
                offset: 5,
                messages: vec![1, 2, 3],
            },
        );
        assert_eq!(
            history,
            AffectedHistory {
                pts: 11,
                pts_count: 1,
                is_final: false
            }
        );
    }

    #[test]
    fn the_amount_clamp_matches_ton_and_star_count() {
        let max = 1i64 << 51;
        assert_eq!(get_amount_count(5, false), 5);
        assert_eq!(get_amount_count(-5, false), 0);
        assert_eq!(get_amount_count(-5, true), -5);
        assert_eq!(get_amount_count(max + 1, false), max);
        assert_eq!(get_amount_count(-max - 1, true), -max);
    }

    #[test]
    fn emoji_game_info_intake_rejects_bad_shapes() {
        let good = telegram_api::messages_EmojiGameInfo::messages_emojiGameDiceInfo(
            telegram_api::messages_emojiGameDiceInfo {
                flags: 0,
                game_hash: "hash".to_owned(),
                prev_stake: 1500000000,
                current_streak: 2,
                params: vec![1, 2, 3, 4, 5, 6, 7],
                plays_left: 0,
            },
        );
        let info = EmojiGameInfo::from_wire(&good);
        assert_eq!(
            info,
            EmojiGameInfo {
                game_hash: "hash".to_owned(),
                prev_stake: 1500000000,
                current_streak: 2,
                params: vec![1, 2, 3, 4, 5, 6, 7],
            }
        );

        // the unavailable constructor resets to the default record
        let unavailable = telegram_api::messages_EmojiGameInfo::messages_emojiGameUnavailable(
            telegram_api::messages_emojiGameUnavailable {},
        );
        assert_eq!(
            EmojiGameInfo::from_wire(&unavailable),
            EmojiGameInfo::default()
        );

        // each bad-shape branch falls back to the default
        let bad = |game_hash: &str, prev_stake: i64, current_streak: i32, params: Vec<i32>| {
            telegram_api::messages_EmojiGameInfo::messages_emojiGameDiceInfo(
                telegram_api::messages_emojiGameDiceInfo {
                    flags: 0,
                    game_hash: game_hash.to_owned(),
                    prev_stake,
                    current_streak,
                    params,
                    plays_left: 0,
                },
            )
        };
        assert_eq!(
            EmojiGameInfo::from_wire(&bad("", 1, 0, vec![1; 7])),
            EmojiGameInfo::default()
        );
        assert_eq!(
            EmojiGameInfo::from_wire(&bad("h", -1, 0, vec![1; 7])),
            EmojiGameInfo::default()
        );
        assert_eq!(
            EmojiGameInfo::from_wire(&bad("h", 1, 3, vec![1; 7])),
            EmojiGameInfo::default()
        );
        assert_eq!(
            EmojiGameInfo::from_wire(&bad("h", 1, 0, vec![1; 6])),
            EmojiGameInfo::default()
        );
        assert_eq!(
            EmojiGameInfo::from_wire(&bad("h", 1, 0, vec![-1, 1, 1, 1, 1, 1, 1])),
            EmojiGameInfo::default()
        );
    }

    #[test]
    fn emoji_game_info_builds_the_stake_dice_state() {
        let mut env = TestEnv::default();
        env.options.insert(
            "ton_stakedice_stake_suggested_amounts".to_owned(),
            "1,2,3".to_owned(),
        );
        let info = EmojiGameInfo {
            game_hash: "hash".to_owned(),
            prev_stake: 2500000000,
            current_streak: 1,
            params: vec![10, 20, 30, 40, 50, 60, 70],
        };
        let state = info.get_stake_dice_state_object(&env);
        assert_eq!(state.state_hash, "hash");
        assert_eq!(state.stake_gram_amount, 2500000000);
        assert_eq!(state.suggested_stake_gram_amounts, vec![1, 2, 3]);
        assert_eq!(state.current_streak, 1);
        assert_eq!(state.prize_per_mille, vec![10, 20, 30, 40, 50, 60]);
        assert_eq!(state.streak_prize_per_mille, 70);

        // the update wraps the same state
        let td_api::Update::updateStakeDiceState(update) =
            info.get_update_stake_dice_state_object(&env)
        else {
            panic!("wrong update constructor");
        };
        assert_eq!(update.state.unwrap(), state);

        // the empty record builds the default state without reading options
        let state = EmojiGameInfo::default().get_stake_dice_state_object(&env);
        assert_eq!(state, td_api::StakeDiceState::default());
    }

    #[test]
    fn search_posts_flood_builds_the_limits_object() {
        let env = TestEnv {
            unix_time: 1000,
            ..TestEnv::default()
        };
        let flood = SearchPostsFlood::from_wire(&telegram_api::SearchPostsFlood {
            flags: 1, // query_is_free
            query_is_free: true,
            total_daily: 100,
            remains: 3,
            wait_till: 1600,
            stars_amount: 50,
        });
        assert!(flood.is_free());
        assert_eq!(
            flood.get_public_post_search_limits_object(&env),
            td_api::PublicPostSearchLimits {
                daily_free_query_count: 100,
                remaining_free_query_count: 3,
                next_free_query_in: 600,
                star_count: 50,
                is_current_query_free: true,
            }
        );

        // the next-free counter clamps at zero and the star count clamps at 2^51
        let exhausted = SearchPostsFlood::from_wire(&telegram_api::SearchPostsFlood {
            flags: 0,
            query_is_free: false,
            total_daily: 100,
            remains: 0,
            wait_till: 5,
            stars_amount: (1i64 << 51) + 5,
        });
        assert!(!exhausted.is_free());
        assert_eq!(
            exhausted.get_public_post_search_limits_object(&env),
            td_api::PublicPostSearchLimits {
                daily_free_query_count: 100,
                remaining_free_query_count: 0,
                next_free_query_in: 0,
                star_count: 1i64 << 51,
                is_current_query_free: false,
            }
        );
    }

    #[test]
    fn message_search_offset_round_trips_and_validates() {
        // the valid form round-trips through the wire string
        let offset = MessageSearchOffset::from_offset_string("100,777,7").unwrap();
        assert_eq!(offset.to_offset_string(), "100,777,7");

        // the empty offset is the unbounded form
        let unbounded = MessageSearchOffset::from_offset_string("").unwrap();
        assert_eq!(unbounded.to_offset_string(), "2147483647,0,0");

        // the invalid bands
        for bad in [
            "1,2",
            "1,2,3,4",
            "x,777,7",
            "100,777,x",
            "0,777,7",
            "100,0,7",
            "100,777,0",
        ] {
            let error = MessageSearchOffset::from_offset_string(bad).unwrap_err();
            assert_eq!(error.code(), 400);
            assert_eq!(error.message(), "Invalid offset specified");
        }
    }

    #[test]
    fn message_search_offset_updates_from_messages() {
        let mut offset = MessageSearchOffset::default();
        // an empty message carries no position and is skipped
        offset.update_from_message(&telegram_api::Message::messageEmpty(
            telegram_api::messageEmpty {
                flags: 0,
                ..Default::default()
            },
        ));
        assert_eq!(offset.to_offset_string(), "0,0,0");

        // a usable message sets the position; a later one overwrites it
        offset.update_from_message(&sample_message(5, 1000, 777));
        assert_eq!(offset.to_offset_string(), "1000,777,5");
        offset.update_from_message(&sample_message(6, 1001, 777));
        assert_eq!(offset.to_offset_string(), "1001,777,6");
    }

    /// The logevent round-trip helper: version-prefix + store + parse, with
    /// the trailing-bytes pin (`fetch_end`) to catch framing drift.
    fn round_trip<T: Default + PartialEq + std::fmt::Debug>(
        log_event: T,
        store: fn(&T, &mut Vec<u8>),
        parse: fn(&mut Parser<'_>) -> Result<T, &'static str>,
        expected_len: Option<usize>,
    ) -> T {
        let mut blob = Vec::new();
        store_log_event_version(&mut blob);
        store(&log_event, &mut blob);
        if let Some(expected_len) = expected_len {
            assert_eq!(blob.len(), 4 + expected_len, "byte-length pin failed");
        }
        let mut parser = Parser::new_log_event(&blob).unwrap();
        let parsed = parse(&mut parser).unwrap();
        parser.fetch_end().unwrap();
        assert_eq!(parsed, log_event);
        parsed
    }

    #[test]
    fn block_message_sender_from_replies_log_event_round_trips() {
        round_trip(
            BlockMessageSenderFromRepliesOnServerLogEvent {
                message_id: MessageId::from_server_message_id(ServerMessageId::new(9)),
                delete_message: true,
                delete_all_messages: false,
                report_spam: true,
            },
            |value, out| value.store(out),
            BlockMessageSenderFromRepliesOnServerLogEvent::parse,
            // flags word + message id
            Some(4 + 8),
        );
    }

    #[test]
    fn delete_all_call_messages_log_event_round_trips() {
        round_trip(
            DeleteAllCallMessagesOnServerLogEvent { revoke: true },
            |value, out| value.store(out),
            DeleteAllCallMessagesOnServerLogEvent::parse,
            Some(4),
        );
    }

    #[test]
    fn delete_all_channel_messages_from_sender_log_event_round_trips() {
        // the current form stores the sender as a dialog id
        round_trip(
            DeleteAllChannelMessagesFromSenderOnServerLogEvent {
                channel_id: ChannelId::new(1234),
                sender_dialog_id: chat_dialog(500),
            },
            |value, out| value.store(out),
            DeleteAllChannelMessagesFromSenderOnServerLogEvent::parse,
            Some(8 + 8),
        );

        // the pre-AddKeyboardButtonFlags form stores a bare user id
        let mut blob = Vec::new();
        store_parse::store_i32(&mut blob, 34); // Version 34 < AddKeyboardButtonFlags
        store_parse::store_i64(&mut blob, 1234);
        store_parse::store_i64(&mut blob, 42); // the user id
        let mut parser = Parser::new_log_event(&blob).unwrap();
        let parsed =
            DeleteAllChannelMessagesFromSenderOnServerLogEvent::parse(&mut parser).unwrap();
        parser.fetch_end().unwrap();
        assert_eq!(parsed.channel_id, ChannelId::new(1234));
        assert_eq!(parsed.sender_dialog_id, user_dialog(42));
    }

    #[test]
    fn delete_dialog_history_log_event_round_trips() {
        round_trip(
            DeleteDialogHistoryOnServerLogEvent {
                dialog_id: chat_dialog(600),
                max_message_id: MessageId::from_server_message_id(ServerMessageId::new(11)),
                remove_from_dialog_list: true,
                revoke: false,
            },
            |value, out| value.store(out),
            DeleteDialogHistoryOnServerLogEvent::parse,
            Some(4 + 8 + 8),
        );
    }

    #[test]
    fn delete_dialog_messages_by_date_log_event_round_trips() {
        round_trip(
            DeleteDialogMessagesByDateOnServerLogEvent {
                dialog_id: user_dialog(43),
                min_date: 1376438400,
                max_date: 1600000000,
                revoke: true,
            },
            |value, out| value.store(out),
            DeleteDialogMessagesByDateOnServerLogEvent::parse,
            Some(4 + 8 + 4 + 4),
        );
    }

    #[test]
    fn delete_messages_log_event_round_trips() {
        round_trip(
            DeleteMessagesOnServerLogEvent {
                dialog_id: user_dialog(44),
                message_ids: vec![
                    MessageId::from_server_message_id(ServerMessageId::new(1)),
                    MessageId::from_server_message_id(ServerMessageId::new(2)),
                ],
                revoke: true,
            },
            |value, out| value.store(out),
            DeleteMessagesOnServerLogEvent::parse,
            Some(4 + 8 + 4 + 2 * 8),
        );
    }

    #[test]
    fn delete_scheduled_messages_log_event_round_trips() {
        // no flag block on this one
        round_trip(
            DeleteScheduledMessagesOnServerLogEvent {
                dialog_id: user_dialog(45),
                message_ids: vec![MessageId::from_server_message_id(ServerMessageId::new(3))],
            },
            |value, out| value.store(out),
            DeleteScheduledMessagesOnServerLogEvent::parse,
            Some(8 + 4 + 8),
        );
    }

    #[test]
    fn delete_ephemeral_message_log_event_round_trips() {
        round_trip(
            DeleteEphemeralMessageOnServerLogEvent {
                dialog_id: user_dialog(46),
                receiver_dialog_id: user_dialog(47),
                ephemeral_message_id: 12,
            },
            |value, out| value.store(out),
            DeleteEphemeralMessageOnServerLogEvent::parse,
            Some(8 + 8 + 4),
        );
    }

    #[test]
    fn delete_topic_history_log_event_round_trips() {
        round_trip(
            DeleteTopicHistoryOnServerLogEvent {
                dialog_id: user_dialog(48),
                forum_topic_id: crate::files::ForumTopicId::new(5),
            },
            |value, out| value.store(out),
            DeleteTopicHistoryOnServerLogEvent::parse,
            // the empty flags word is still stored
            Some(4 + 8 + 4),
        );
    }

    #[test]
    fn read_all_dialog_mentions_log_event_round_trips() {
        round_trip(
            ReadAllDialogMentionsOnServerLogEvent {
                dialog_id: user_dialog(49),
            },
            |value, out| value.store(out),
            ReadAllDialogMentionsOnServerLogEvent::parse,
            Some(8),
        );
    }

    #[test]
    fn read_all_dialog_reactions_log_event_round_trips() {
        round_trip(
            ReadAllDialogReactionsOnServerLogEvent {
                dialog_id: chat_dialog(700),
            },
            |value, out| value.store(out),
            ReadAllDialogReactionsOnServerLogEvent::parse,
            Some(8),
        );
    }

    #[test]
    fn read_all_poll_votes_log_event_round_trips() {
        // with a valid topic id
        round_trip(
            ReadAllPollVotesOnServerLogEvent {
                dialog_id: user_dialog(50),
                forum_topic_id: crate::files::ForumTopicId::new(6),
            },
            |value, out| value.store(out),
            ReadAllPollVotesOnServerLogEvent::parse,
            Some(4 + 8 + 4),
        );

        // without one — the field is omitted from the bytes
        round_trip(
            ReadAllPollVotesOnServerLogEvent {
                dialog_id: user_dialog(50),
                forum_topic_id: crate::files::ForumTopicId::new(0),
            },
            |value, out| value.store(out),
            ReadAllPollVotesOnServerLogEvent::parse,
            Some(4 + 8),
        );
    }

    #[test]
    fn read_message_contents_log_event_round_trips() {
        round_trip(
            ReadMessageContentsOnServerLogEvent {
                dialog_id: user_dialog(51),
                message_ids: vec![
                    MessageId::from_server_message_id(ServerMessageId::new(7)),
                    MessageId::from_server_message_id(ServerMessageId::new(8)),
                    MessageId::from_server_message_id(ServerMessageId::new(9)),
                ],
            },
            |value, out| value.store(out),
            ReadMessageContentsOnServerLogEvent::parse,
            Some(8 + 4 + 3 * 8),
        );
    }

    #[test]
    fn unpin_all_dialog_messages_log_event_round_trips() {
        round_trip(
            UnpinAllDialogMessagesOnServerLogEvent {
                dialog_id: user_dialog(52),
            },
            |value, out| value.store(out),
            UnpinAllDialogMessagesOnServerLogEvent::parse,
            Some(8),
        );
    }

    #[test]
    fn the_log_event_type_names_are_the_handler_identities() {
        assert_eq!(
            MessageQueryLogEventType::DeleteMessagesOnServer.to_string(),
            "DeleteMessagesOnServer"
        );
        assert_eq!(
            MessageQueryLogEventType::BlockMessageSenderFromRepliesOnServer.to_string(),
            "BlockMessageSenderFromRepliesOnServer"
        );
    }

    // ---------------------------------------------------------------------------
    // M.18.3 machinery tests — the extended offline env records every seam call.
    // ---------------------------------------------------------------------------

    impl RecordingEnv {
        fn recorded(&self, prefix: &str) -> Vec<&str> {
            self.sent_queries
                .iter()
                .map(String::as_str)
                .filter(|name| name.starts_with(prefix))
                .collect()
        }
    }

    /// The extended env: records every seam call by name (with payloads where a
    /// test pins them), drives `now` manually, and overrides the dialog-plane
    /// error bands to the `Ok` halves the tests need.
    type MetricRow = (i64, i32, i32, i32, i32);

    #[derive(Default)]
    struct RecordingEnv {
        base: TestEnv,
        sent_queries: Vec<String>,
        updates: Vec<td_api::Update>,
        pts_updates: Vec<(i32, i32)>,
        channel_pts_updates: Vec<(i32, i32)>,
        now: f64,
        have_message: bool,
        input_peer: bool,
        viewer_source_id: crate::files::FileSourceId,
        metrics: Vec<(DialogId, Vec<MetricRow>)>,
        reactions_sent: Vec<(DialogId, Vec<i64>)>,
        music_listen_document: Option<telegram_api::InputDocument>,
        input_user_ok: bool,
        /// admits every dialog through the access gate (the M.mp tests
        /// that exercise the post-gate bands)
        access_all: bool,
    }

    impl MessageQueryManagerEnv for RecordingEnv {
        fn auth_is_bot(&self) -> bool {
            self.base.is_bot
        }

        fn close_flag(&self) -> bool {
            false
        }

        fn unix_time(&self) -> i32 {
            self.base.unix_time
        }

        fn get_option_string(&self, name: &str, default_value: &str) -> String {
            self.base.get_option_string(name, default_value)
        }

        fn get_user_id_object(&mut self, user_id: UserId, source: &str) -> i64 {
            self.base.get_user_id_object(user_id, source)
        }

        fn have_user_force(&mut self, user_id: UserId, source: &str) -> bool {
            self.base.have_user_force(user_id, source)
        }

        fn now(&self) -> f64 {
            self.now
        }

        fn secure_random_i64(&self) -> i64 {
            424242
        }

        fn check_dialog_access(
            &self,
            dialog_id: DialogId,
            _require_valid: bool,
            _access_rights: crate::user_manager::AccessRights,
            _source: &str,
        ) -> Result<(), tdutils::status::Status> {
            if self.access_all || dialog_id == user_dialog(777) {
                Ok(())
            } else {
                Err(tdutils::status::Status::error(400, "Chat not found"))
            }
        }

        fn have_input_peer(
            &self,
            _dialog_id: DialogId,
            _from_group_chat: bool,
            _access_rights: crate::user_manager::AccessRights,
        ) -> bool {
            self.input_peer
        }

        fn have_message_force(&mut self, _message_full_id: MessageFullId, _source: &str) -> bool {
            self.have_message
        }

        fn can_get_message_viewers(
            &self,
            _message_full_id: MessageFullId,
        ) -> Result<(), tdutils::status::Status> {
            Ok(())
        }

        fn create_rich_message_file_source(
            &mut self,
            _message_full_id: MessageFullId,
        ) -> crate::files::FileSourceId {
            self.viewer_source_id
        }

        fn add_pending_pts_update(
            &mut self,
            pts: i32,
            pts_count: i32,
            _receive_time: f64,
            _promise: Promise<()>,
            _source: &str,
        ) {
            self.pts_updates.push((pts, pts_count));
        }

        fn add_pending_channel_update(
            &mut self,
            _dialog_id: DialogId,
            pts: i32,
            pts_count: i32,
            _promise: Promise<()>,
            _source: &str,
        ) {
            self.channel_pts_updates.push((pts, pts_count));
        }

        fn send_report_read_metrics_query(
            &mut self,
            dialog_id: DialogId,
            metrics: Vec<MessageReadMetric>,
        ) {
            let rows: Vec<MetricRow> = metrics
                .into_iter()
                .map(|metric| {
                    (
                        metric.view_id,
                        metric.time_in_view_ms,
                        metric.active_time_in_view_ms,
                        metric.height_to_viewport_ratio_per_mille,
                        metric.seen_range_ratio_per_mille,
                    )
                })
                .collect();
            self.metrics.push((dialog_id, rows));
        }

        fn send_get_messages_reactions_query(
            &mut self,
            dialog_id: DialogId,
            message_ids: Vec<MessageId>,
        ) {
            self.reactions_sent.push((
                dialog_id,
                message_ids
                    .iter()
                    .map(|id| i64::from(id.get_server_message_id().get()))
                    .collect(),
            ));
            self.sent_queries
                .push("GetMessagesReactionsQuery".to_owned());
        }

        fn send_affected_history_query(
            &mut self,
            _dialog_id: DialogId,
            _query: AffectedHistoryQueryKind,
            _get_affected_messages: bool,
            _promise: Promise<()>,
        ) {
            self.sent_queries.push("AffectedHistoryQuery".to_owned());
        }

        fn get_input_user(
            &mut self,
            _user_id: UserId,
        ) -> Result<telegram_api::InputUser, tdutils::status::Status> {
            if self.input_user_ok {
                Ok(telegram_api::InputUser::inputUserSelf(
                    telegram_api::inputUserSelf {},
                ))
            } else {
                Err(tdutils::status::Status::error(400, "User not found"))
            }
        }

        fn get_music_listen_input_document(
            &mut self,
            _file_id: crate::files::FileId,
        ) -> Option<telegram_api::InputDocument> {
            self.music_listen_document.clone()
        }

        fn send_search_messages_global_query(
            &mut self,
            _folder_id: crate::messages_manager::FolderId,
            _ignore_folder_id: bool,
            _query: String,
            _offset_date: i32,
            _offset_dialog_id: DialogId,
            _offset_message_id: MessageId,
            _limit: i32,
            _filter: crate::messages_manager::MessageSearchFilter,
            _chat_type_filter: Option<td_api::SearchMessagesChatTypeFilter>,
            _min_date: i32,
            _max_date: i32,
            _promise: Promise<td_api::FoundMessages>,
        ) {
            self.sent_queries
                .push("SearchMessagesGlobalQuery".to_owned());
        }

        fn send_search_posts_query(
            &mut self,
            hashtag: String,
            _offset: MessageSearchOffset,
            _limit: i32,
            _promise: Promise<td_api::FoundMessages>,
        ) {
            self.sent_queries
                .push(format!("SearchPostsQuery({hashtag})"));
        }

        fn hashtag_used(&mut self, is_cashtag: bool, hashtag: String) {
            self.sent_queries.push(format!(
                "HashtagUsed({},{hashtag})",
                if is_cashtag { "$" } else { "#" }
            ));
        }

        fn send_get_personal_channel_history_query(
            &mut self,
            _input_user: telegram_api::InputUser,
            _limit: i32,
            _promise: Promise<td_api::Messages>,
        ) {
            self.sent_queries
                .push("GetPersonalChannelHistoryQuery".to_owned());
        }

        fn send_report_music_listen_query(
            &mut self,
            _input_document: telegram_api::InputDocument,
            _duration: i32,
            _promise: Promise<()>,
        ) {
            self.sent_queries.push("ReportMusicListenQuery".to_owned());
        }

        fn send_get_messages_views_query(
            &mut self,
            _dialog_id: DialogId,
            message_ids: Vec<MessageId>,
            increment_view_counter: bool,
        ) {
            self.sent_queries.push(format!(
                "GetMessagesViewsQuery({increment_view_counter}, {})",
                message_ids.len()
            ));
        }

        fn send_read_message_contents_query(
            &mut self,
            _dialog_id: DialogId,
            _message_ids: Vec<MessageId>,
        ) {
            self.sent_queries
                .push("ReadMessageContentsQuery".to_owned());
        }

        fn send_update(&mut self, update: td_api::Update) {
            self.updates.push(update);
        }
    }

    #[test]
    fn fix_delete_message_min_max_dates_normalizes_the_window() {
        let env = TestEnv {
            unix_time: 1700000000,
            ..TestEnv::default()
        };
        let error =
            MessageQueryManager::fix_delete_message_min_max_dates(&env, 100, 50).unwrap_err();
        assert_eq!(error.code(), 400);
        assert_eq!(error.message(), "Wrong date interval specified");

        // everything below the launch date zeroes out
        assert_eq!(
            MessageQueryManager::fix_delete_message_min_max_dates(&env, 1, 1376438399).unwrap(),
            (0, 0)
        );

        // the min date clamps up to the launch date
        assert_eq!(
            MessageQueryManager::fix_delete_message_min_max_dates(&env, 100, 1400000000).unwrap(),
            (1376438400, 1400000000)
        );

        // the recent window zeroes out
        assert_eq!(
            MessageQueryManager::fix_delete_message_min_max_dates(
                &env,
                1700000000 - 20,
                1700000000
            )
            .unwrap(),
            (0, 0)
        );

        // the max date clamps down below the last 30 seconds
        assert_eq!(
            MessageQueryManager::fix_delete_message_min_max_dates(&env, 1376438400, 1700000000)
                .unwrap(),
            (1376438400, 1700000000 - 31)
        );
    }

    #[test]
    fn the_affected_history_loop_books_pts_and_continues() {
        let mut manager = MessageQueryManager::new();
        let mut env = RecordingEnv::default();

        // a partial answer books the pts update and re-sends the query
        let promise = Promise::<()>::default();
        let _ = &promise;
        manager.on_get_affected_history(
            &mut env,
            user_dialog(777),
            AffectedHistoryQueryKind::ReadAllDialogMentions,
            false,
            AffectedHistory {
                pts: 10,
                pts_count: 2,
                is_final: false,
            },
            promise,
        );
        assert_eq!(env.pts_updates, vec![(10, 2)]);
        assert_eq!(env.recorded("AffectedHistoryQuery").len(), 1);

        // the get_affected_messages form zeroes the booked pts_count
        let promise = Promise::<()>::default();
        manager.on_get_affected_history(
            &mut env,
            user_dialog(777),
            AffectedHistoryQueryKind::ReadAllDialogMentions,
            true,
            AffectedHistory {
                pts: 11,
                pts_count: 2,
                is_final: false,
            },
            promise,
        );
        assert_eq!(env.pts_updates.last(), Some(&(11, 0)));

        // a channel dialog rides the channel update path
        let promise = Promise::<()>::default();
        manager.on_get_affected_history(
            &mut env,
            DialogId::from_channel_id(crate::chat_manager::ChannelId::new(42)),
            AffectedHistoryQueryKind::ReadAllDialogMentions,
            false,
            AffectedHistory {
                pts: 12,
                pts_count: 1,
                is_final: false,
            },
            promise,
        );
        assert_eq!(env.channel_pts_updates, vec![(12, 1)]);

        // the final answer with no bookkeeping resolves the promise
        let promise = Promise::<()>::default();
        assert!(promise.is_empty());
        manager.on_get_affected_history(
            &mut env,
            user_dialog(777),
            AffectedHistoryQueryKind::ReadAllDialogMentions,
            false,
            AffectedHistory {
                pts: 0,
                pts_count: 0,
                is_final: true,
            },
            promise,
        );

        // the final answer with bookkeeping hands the promise to the pts update
        let promise = Promise::<()>::default();
        manager.on_get_affected_history(
            &mut env,
            user_dialog(777),
            AffectedHistoryQueryKind::ReadAllDialogMentions,
            false,
            AffectedHistory {
                pts: 13,
                pts_count: 1,
                is_final: true,
            },
            promise,
        );
    }

    #[test]
    fn the_reactions_reload_queue_drains_and_gates() {
        let mut manager = MessageQueryManager::new();
        let mut env = RecordingEnv {
            input_peer: true,
            ..RecordingEnv::default()
        };

        let ids: Vec<MessageId> = (1..=3)
            .map(|id| MessageId::from_server_message_id(ServerMessageId::new(id)))
            .collect();
        manager.queue_message_reactions_reload_for_dialog(&mut env, user_dialog(777), &ids);
        assert_eq!(env.reactions_sent.len(), 1);
        assert_eq!(env.reactions_sent[0].1, vec![1, 2, 3]);

        // the request flag holds further queues until the result arrives
        manager.queue_message_reactions_reload_for_dialog(&mut env, user_dialog(777), &ids);
        assert_eq!(env.reactions_sent.len(), 1);

        // the finished flag re-drains the requeued ids, then the empty state erases
        manager.try_reload_message_reactions(&mut env, user_dialog(777), true);
        assert_eq!(env.reactions_sent.len(), 2);
        manager.try_reload_message_reactions(&mut env, user_dialog(777), true);
        assert!(manager.being_reloaded_reactions.is_empty());

        // a pending read-reactions id is filtered out of the send
        let pending_id = MessageId::from_server_message_id(ServerMessageId::new(1));
        manager
            .pending_read_reactions
            .insert(MessageFullId::new(user_dialog(777), pending_id), 1);
        manager.queue_message_reactions_reload(
            &mut env,
            MessageFullId::new(
                user_dialog(777),
                MessageId::from_server_message_id(ServerMessageId::new(2)),
            ),
        );
        assert_eq!(env.reactions_sent.last().unwrap().1, vec![2]);

        // the no-input-peer path parks a 0.2s retry instead of sending
        let mut env = RecordingEnv {
            input_peer: false,
            ..RecordingEnv::default()
        };
        manager.try_reload_message_reactions(&mut env, user_dialog(777), true);
        // the no-input-peer finished path sends nothing
        assert_eq!(env.reactions_sent.len(), 0);
        manager.queue_message_reactions_reload(
            &mut env,
            MessageFullId::new(
                user_dialog(777),
                MessageId::from_server_message_id(ServerMessageId::new(5)),
            ),
        );
        assert_eq!(
            manager
                .reactions_reload_retry_deadlines
                .get(&user_dialog(777)),
            Some(&(env.now + 0.2))
        );
        // the retry fires at the deadline and re-enters the finished path
        env.now += 0.3;
        manager.tick_timeouts(&mut env);
        assert!(manager.reactions_reload_retry_deadlines.is_empty());
    }

    #[test]
    fn the_reactions_reload_queue_chunks_at_the_server_limit() {
        let mut manager = MessageQueryManager::new();
        let mut env = RecordingEnv {
            input_peer: true,
            ..RecordingEnv::default()
        };
        let ids: Vec<MessageId> = (1..=150)
            .map(|id| MessageId::from_server_message_id(ServerMessageId::new(id)))
            .collect();
        manager.queue_message_reactions_reload_for_dialog(&mut env, user_dialog(777), &ids);
        assert_eq!(env.reactions_sent.len(), 1);
        assert_eq!(env.reactions_sent[0].1.len(), 100);
        // finishing the sent request drains the remainder
        manager.try_reload_message_reactions(&mut env, user_dialog(777), true);
        assert_eq!(env.reactions_sent.len(), 2);
        assert_eq!(env.reactions_sent[1].1.len(), 50);
    }

    #[test]
    fn the_pending_read_counters_track_and_reload_on_error() {
        let mut manager = MessageQueryManager::new();
        let mut env = RecordingEnv {
            input_peer: true,
            ..RecordingEnv::default()
        };
        let message_full_id = MessageFullId::new(
            user_dialog(777),
            MessageId::from_server_message_id(ServerMessageId::new(9)),
        );
        manager.read_message_reactions_on_server(
            &mut env,
            user_dialog(777),
            vec![message_full_id.get_message_id()],
        );
        assert!(manager.has_message_pending_read_reactions(message_full_id));
        assert!(env.recorded("ReadMessageContentsQuery").len() == 1);

        // the ok result clears the counter
        manager.on_read_message_reactions(
            &mut env,
            user_dialog(777),
            vec![message_full_id.get_message_id()],
            false,
        );
        assert!(!manager.has_message_pending_read_reactions(message_full_id));

        // the error result requeues a reload for a live message...
        manager.read_message_reactions_on_server(
            &mut env,
            user_dialog(777),
            vec![message_full_id.get_message_id()],
        );
        env.have_message = true;
        manager.on_read_message_reactions(
            &mut env,
            user_dialog(777),
            vec![message_full_id.get_message_id()],
            true,
        );
        assert_eq!(env.recorded("GetMessagesReactionsQuery").len(), 1);
        assert!(!manager.has_message_pending_read_reactions(message_full_id));

        // ...and skips it for an unknown one
        manager.read_message_reactions_on_server(
            &mut env,
            user_dialog(777),
            vec![message_full_id.get_message_id()],
        );
        let send_count = env.recorded("GetMessagesReactionsQuery").len();
        manager.on_read_message_reactions(
            &mut env,
            user_dialog(777),
            vec![message_full_id.get_message_id()],
            true,
        );
        assert_eq!(env.recorded("GetMessagesReactionsQuery").len(), send_count);

        // the poll-vote counters mirror the machinery
        manager.read_message_poll_votes_on_server(
            &mut env,
            user_dialog(777),
            vec![message_full_id.get_message_id()],
        );
        assert!(manager.has_message_pending_read_poll_votes(message_full_id));
        manager.on_read_message_poll_votes(
            &mut env,
            user_dialog(777),
            vec![message_full_id.get_message_id()],
            false,
        );
        assert!(!manager.has_message_pending_read_poll_votes(message_full_id));
    }

    #[test]
    fn send_message_view_metrics_validates_accumulates_and_folds() {
        let mut manager = MessageQueryManager::new();
        let mut env = RecordingEnv::default();
        let message_id = MessageId::from_server_message_id(ServerMessageId::new(11));

        // the access gate answers the dialog-plane error band
        let promise = Promise::<()>::default();
        manager.send_message_view_metrics(
            &mut env,
            user_dialog(1),
            message_id,
            500,
            100,
            100,
            100,
            promise,
        );
        assert!(env.metrics.is_empty());

        // the invalid band
        let promise = Promise::<()>::default();
        manager.send_message_view_metrics(
            &mut env,
            user_dialog(777),
            message_id,
            -1,
            100,
            100,
            100,
            promise,
        );
        // the seen-range cap
        let promise = Promise::<()>::default();
        manager.send_message_view_metrics(
            &mut env,
            user_dialog(777),
            message_id,
            500,
            100,
            100,
            1001,
            promise,
        );
        assert!(env.metrics.is_empty());
        assert!(manager.pending_message_view_metrics.is_empty());

        // the below-300ms and non-server answers are ok without buffering
        env.have_message = true;
        let promise = Promise::<()>::default();
        manager.send_message_view_metrics(
            &mut env,
            user_dialog(777),
            message_id,
            299,
            100,
            100,
            100,
            promise,
        );
        let promise = Promise::<()>::default();
        manager.send_message_view_metrics(
            &mut env,
            user_dialog(777),
            MessageId::new(2), // a local id, not a server one
            500,
            100,
            100,
            100,
            promise,
        );
        assert!(env.metrics.is_empty());
        assert!(manager.pending_message_view_metrics.is_empty());

        // the live path buffers and re-arms the 5.0s deadline
        let promise = Promise::<()>::default();
        manager.send_message_view_metrics(
            &mut env,
            user_dialog(777),
            message_id,
            500,
            100,
            40,
            500,
            promise,
        );
        assert_eq!(
            manager.pending_message_view_metrics[&user_dialog(777)].len(),
            1
        );
        assert_eq!(
            manager.view_metrics_deadlines[&user_dialog(777)],
            env.now + 5.0
        );

        // the fold: summed times, max ratios, one metric per message
        manager.send_message_view_metrics(
            &mut env,
            user_dialog(777),
            message_id,
            700,
            300,
            60,
            400,
            Promise::<()>::default(),
        );
        env.now += 6.0;
        manager.tick_timeouts(&mut env);
        assert_eq!(env.metrics.len(), 1);
        assert_eq!(env.metrics[0].0, user_dialog(777));
        assert_eq!(
            env.metrics[0].1,
            vec![(424242, 500 + 700, 100 + 300, 60, 500)]
        );
        assert!(manager.pending_message_view_metrics.is_empty());
        assert!(manager.next_deadline().is_none());
    }

    #[test]
    fn view_messages_dedups_chunks_and_finishes() {
        let mut manager = MessageQueryManager::new();
        let mut env = RecordingEnv::default();
        let ids: Vec<MessageId> = (1..=105)
            .map(|id| MessageId::from_server_message_id(ServerMessageId::new(id)))
            .collect();

        manager.view_messages(&mut env, user_dialog(777), &ids, true);
        assert_eq!(env.recorded("GetMessagesViewsQuery").len(), 2);
        assert_eq!(manager.being_reloaded_views_message_full_ids.len(), 105);
        assert_eq!(
            manager.need_view_counter_increment_message_full_ids.len(),
            105
        );

        // the dedup: everything is already being reloaded, nothing sends
        manager.view_messages(&mut env, user_dialog(777), &ids, true);
        assert_eq!(env.recorded("GetMessagesViewsQuery").len(), 2);

        // the finish clears the sets and re-allows the view
        manager.finish_get_message_views(user_dialog(777), &ids);
        assert!(manager.being_reloaded_views_message_full_ids.is_empty());
        assert!(
            manager
                .need_view_counter_increment_message_full_ids
                .is_empty()
        );
        manager.view_messages(&mut env, user_dialog(777), &ids, false);
        assert_eq!(env.recorded("GetMessagesViewsQuery").len(), 4);
        assert_eq!(
            env.sent_queries
                .iter()
                .filter(|query| query.contains("(false,"))
                .count(),
            2
        );
    }

    #[test]
    fn the_emoji_game_cache_updates_and_answers_the_state() {
        let mut manager = MessageQueryManager::new();
        let mut env = RecordingEnv::default();
        env.base.options.insert(
            "ton_stakedice_stake_suggested_amounts".to_owned(),
            "1,2,3".to_owned(),
        );
        let wire = telegram_api::messages_EmojiGameInfo::messages_emojiGameDiceInfo(
            telegram_api::messages_emojiGameDiceInfo {
                flags: 0,
                game_hash: "hash".to_owned(),
                prev_stake: 1500000000,
                current_streak: 1,
                params: vec![1, 2, 3, 4, 5, 6, 7],
                plays_left: 0,
            },
        );

        // bots ignore the update
        env.base.is_bot = true;
        manager.on_update_emoji_game_info(&mut env, &wire);
        assert!(!manager.is_emoji_game_info_inited);
        assert!(env.updates.is_empty());
        env.base.is_bot = false;

        // the first update caches and emits the update
        manager.on_update_emoji_game_info(&mut env, &wire);
        assert!(manager.is_emoji_game_info_inited);
        assert_eq!(env.updates.len(), 1);

        // the same value is a no-op
        manager.on_update_emoji_game_info(&mut env, &wire);
        assert_eq!(env.updates.len(), 1);

        // a changed value re-emits
        let changed = telegram_api::messages_EmojiGameInfo::messages_emojiGameDiceInfo(
            telegram_api::messages_emojiGameDiceInfo {
                flags: 0,
                game_hash: "hash2".to_owned(),
                prev_stake: 1,
                current_streak: 0,
                params: vec![1, 2, 3, 4, 5, 6, 7],
                plays_left: 0,
            },
        );
        manager.on_update_emoji_game_info(&mut env, &changed);
        assert_eq!(env.updates.len(), 2);

        // the state block carries the update only within the 60s window
        manager.emoji_game_info_receive_time = env.now - 30.0;
        let mut updates = Vec::new();
        manager.get_current_state(&env, &mut updates);
        assert_eq!(updates.len(), 1);
        manager.emoji_game_info_receive_time = env.now - 61.0;
        let mut updates = Vec::new();
        manager.get_current_state(&env, &mut updates);
        assert!(updates.is_empty());

        // the fetched answer builds the object
        let promise = Promise::<td_api::StakeDiceState>::default();
        manager.on_get_emoji_game_info(&mut env, &wire, promise);
        assert_eq!(manager.emoji_game_info.game_hash, "hash");
    }

    #[test]
    fn the_rich_message_file_source_cache_is_bot_and_shape_gated() {
        let mut manager = MessageQueryManager::new();
        let mut env = RecordingEnv::default();

        // bots answer the invalid id without touching the cache
        env.base.is_bot = true;
        let id = manager.get_rich_message_file_source_id(
            &mut env,
            MessageFullId::new(
                user_dialog(777),
                MessageId::from_server_message_id(ServerMessageId::new(3)),
            ),
        );
        assert!(!id.is_valid());
        assert!(manager.rich_message_full_id_to_file_source_id.is_empty());
        env.base.is_bot = false;

        // the invalid shapes too
        assert!(
            !manager
                .get_rich_message_file_source_id(
                    &mut env,
                    MessageFullId::new(user_dialog(777), MessageId::new(2)), // local
                )
                .is_valid()
        );
        assert!(manager.rich_message_full_id_to_file_source_id.is_empty());

        // the live shape allocates through the seam and caches
        env.viewer_source_id = crate::files::FileSourceId::new(42);
        let message_full_id = MessageFullId::new(
            user_dialog(777),
            MessageId::from_server_message_id(ServerMessageId::new(3)),
        );
        let id = manager.get_rich_message_file_source_id(&mut env, message_full_id);
        assert_eq!(id.get(), 42);
        assert_eq!(
            manager
                .get_rich_message_file_source_id(&mut env, message_full_id)
                .get(),
            42
        );
    }

    #[test]
    fn the_extended_media_and_fact_check_dedup_sets_gate_the_sends() {
        let mut manager = MessageQueryManager::new();
        let mut env = RecordingEnv::default();
        let ids = vec![MessageId::from_server_message_id(ServerMessageId::new(1))];

        manager.reload_message_extended_media(&mut env, user_dialog(777), ids.clone());
        manager.reload_message_fact_checks(&mut env, user_dialog(777), ids.clone());
        // the C++ shared-set quirk: both reloads filter through the fact-check
        // set (MessageQueryManager.cpp:3264-3265/3281-3283)
        assert_eq!(manager.being_reloaded_fact_checks.len(), 1);
        assert!(
            manager
                .being_reloaded_extended_media_message_full_ids
                .is_empty()
        );

        // the finish erases the extended-media set (a no-op here, like the C++)
        manager.finish_get_message_extended_media(user_dialog(777), &ids);
        manager.on_reload_message_fact_checks(&mut env, user_dialog(777), &ids, false);
        assert!(manager.being_reloaded_fact_checks.is_empty());
    }

    // ------------------------------------------------------------------
    // M.mp request-plane tests — the local validation bands and the
    // search/decision tables over the recording env.
    // ------------------------------------------------------------------

    /// The result-capturing promise interface (the call_manager pattern).
    type MmpCell<T> = std::sync::Arc<std::sync::Mutex<Option<tdutils::status::Result<T>>>>;

    struct MmpRecording<T>(MmpCell<T>);

    impl<T: Send + 'static> tdutils::promise::PromiseInterface<T> for MmpRecording<T> {
        fn set_value(&mut self, value: T) {
            *self.0.lock().unwrap() = Some(tdutils::status::Result::Ok(value));
        }

        fn set_error(&mut self, error: tdutils::status::Status) {
            *self.0.lock().unwrap() = Some(tdutils::status::Result::Err(error));
        }
    }

    fn record<T: Send + 'static>() -> (Promise<T>, MmpCell<T>) {
        let recorded = std::sync::Arc::new(std::sync::Mutex::new(None));
        (
            Promise::from_interface(MmpRecording(recorded.clone())),
            recorded,
        )
    }

    fn recorded_error<T: Send + 'static + std::fmt::Debug>(
        cell: &std::sync::Arc<std::sync::Mutex<Option<tdutils::status::Result<T>>>>,
    ) -> (i32, String) {
        match &*cell.lock().unwrap() {
            Some(tdutils::status::Result::Err(status)) => {
                (status.code(), status.message().to_owned())
            }
            other => panic!("expected an error, got {other:?}"),
        }
    }

    #[test]
    fn the_search_limit_clamp_follows_the_cpp() {
        assert_eq!(
            fix_search_limit(0).unwrap_err().message(),
            "Parameter limit must be positive"
        );
        assert_eq!(
            fix_search_limit(-5).unwrap_err().message(),
            "Parameter limit must be positive"
        );
        assert_eq!(fix_search_limit(5).ok(), Some(5));
        assert_eq!(fix_search_limit(1000).ok(), Some(MAX_SEARCH_MESSAGES));
    }

    #[test]
    fn search_messages_bands_follow_the_cpp() {
        let mut manager = MessageQueryManager::new();
        let mut env = RecordingEnv::default();

        // a non-folder chat list id is rejected
        let (promise, cell) = record::<td_api::FoundMessages>();
        manager.search_messages(
            &mut env,
            crate::messages_manager::DialogListId::from_filter(
                crate::messages_manager::DialogFilterId::new(3),
            ),
            false,
            "query".to_owned(),
            String::new(),
            10,
            crate::messages_manager::MessageSearchFilter::Empty,
            None,
            0,
            0,
            promise,
        );
        assert_eq!(
            recorded_error(&cell),
            (400, "Wrong chat list specified".to_owned())
        );

        // limit must be positive
        let (promise, cell) = record::<td_api::FoundMessages>();
        manager.search_messages(
            &mut env,
            crate::messages_manager::DialogListId::from_folder(
                crate::messages_manager::FolderId::main(),
            ),
            false,
            "query".to_owned(),
            String::new(),
            0,
            crate::messages_manager::MessageSearchFilter::Empty,
            None,
            0,
            0,
            promise,
        );
        assert_eq!(
            recorded_error(&cell),
            (400, "Parameter limit must be positive".to_owned())
        );

        // the unsupported filter band
        let (promise, cell) = record::<td_api::FoundMessages>();
        manager.search_messages(
            &mut env,
            crate::messages_manager::DialogListId::from_folder(
                crate::messages_manager::FolderId::main(),
            ),
            false,
            "query".to_owned(),
            String::new(),
            10,
            crate::messages_manager::MessageSearchFilter::Mention,
            None,
            0,
            0,
            promise,
        );
        assert_eq!(
            recorded_error(&cell),
            (400, "The filter is not supported".to_owned())
        );

        // the empty query + Empty filter band answers with an empty list
        let (promise, cell) = record::<td_api::FoundMessages>();
        manager.search_messages(
            &mut env,
            crate::messages_manager::DialogListId::from_folder(
                crate::messages_manager::FolderId::main(),
            ),
            false,
            String::new(),
            String::new(),
            10,
            crate::messages_manager::MessageSearchFilter::Empty,
            None,
            0,
            0,
            promise,
        );
        match &*cell.lock().unwrap() {
            Some(tdutils::status::Result::Ok(found)) => {
                assert_eq!(found.total_count, 0);
                assert!(found.messages.is_empty());
                assert!(found.next_offset.is_empty());
            }
            other => panic!("expected the empty answer, got {other:?}"),
        }
        assert!(env.recorded("SearchMessagesGlobalQuery").is_empty());

        // otherwise the global query tail parks
        let (promise, _) = record::<td_api::FoundMessages>();
        manager.search_messages(
            &mut env,
            crate::messages_manager::DialogListId::from_folder(
                crate::messages_manager::FolderId::main(),
            ),
            false,
            "query".to_owned(),
            String::new(),
            10,
            crate::messages_manager::MessageSearchFilter::Photo,
            None,
            0,
            0,
            promise,
        );
        assert_eq!(env.recorded("SearchMessagesGlobalQuery").len(), 1);
    }

    #[test]
    fn search_hashtag_posts_splits_and_bands_follow_the_cpp() {
        let mut manager = MessageQueryManager::new();
        let mut env = RecordingEnv::default();

        // limit must be positive
        let (promise, cell) = record::<td_api::FoundMessages>();
        manager.search_hashtag_posts(&mut env, "tag".to_owned(), String::new(), 0, promise);
        assert_eq!(
            recorded_error(&cell),
            (400, "Parameter limit must be positive".to_owned())
        );

        // a bare "#" strips to empty and answers locally
        let (promise, cell) = record::<td_api::FoundMessages>();
        manager.search_hashtag_posts(&mut env, "#".to_owned(), String::new(), 5, promise);
        match &*cell.lock().unwrap() {
            Some(tdutils::status::Result::Ok(found)) => assert_eq!(found.total_count, 0),
            other => panic!("expected the empty answer, got {other:?}"),
        }

        // the hashtag keeps its '#'; the cashtag swaps in '$'
        let (promise, _) = record::<td_api::FoundMessages>();
        manager.search_hashtag_posts(&mut env, "#tag".to_owned(), String::new(), 5, promise);
        assert_eq!(env.recorded("SearchPostsQuery(#tag)").len(), 1);
        assert_eq!(env.recorded("HashtagUsed(#,tag)").len(), 1);

        let (promise, _) = record::<td_api::FoundMessages>();
        manager.search_hashtag_posts(&mut env, "$tag".to_owned(), String::new(), 5, promise);
        assert_eq!(env.recorded("SearchPostsQuery($tag)").len(), 1);
        assert_eq!(env.recorded("HashtagUsed($,tag)").len(), 1);
    }

    #[test]
    fn delete_dialog_messages_by_date_bands_follow_the_cpp() {
        let mut manager = MessageQueryManager::new();
        let mut env = RecordingEnv::default();
        env.base.unix_time = 1_700_000_000;
        env.access_all = true;

        // an inverted window is rejected
        let (promise, cell) = record::<()>();
        manager.delete_dialog_messages_by_date(&mut env, user_dialog(777), 100, 50, false, promise);
        assert_eq!(
            recorded_error(&cell),
            (400, "Wrong date interval specified".to_owned())
        );

        // a window below the launch date collapses to the ok band
        let (promise, cell) = record::<()>();
        manager.delete_dialog_messages_by_date(&mut env, user_dialog(777), 0, 100, false, promise);
        assert!(cell.lock().unwrap().is_some());

        // the channel band rejects the bulk deletion
        let (promise, cell) = record::<()>();
        manager.delete_dialog_messages_by_date(
            &mut env,
            DialogId::from_channel_id(crate::chat_manager::ChannelId::new(42)),
            1_690_000_000,
            1_699_000_000,
            false,
            promise,
        );
        assert_eq!(
            recorded_error(&cell),
            (
                400,
                "Bulk message deletion is unsupported in supergroup chats".to_owned()
            )
        );

        // the basic-group band rejects the revoke
        let (promise, cell) = record::<()>();
        manager.delete_dialog_messages_by_date(
            &mut env,
            DialogId::from_chat_id(crate::chat_manager::ChatId::new(42)),
            1_690_000_000,
            1_699_000_000,
            true,
            promise,
        );
        assert_eq!(
            recorded_error(&cell),
            (
                400,
                "Bulk message revocation is unsupported in basic group chats".to_owned()
            )
        );

        // a user dialog with a live window rides the affected-history loop
        let (promise, _) = record::<()>();
        manager.delete_dialog_messages_by_date(
            &mut env,
            user_dialog(777),
            1_690_000_000,
            1_699_000_000,
            false,
            promise,
        );
        assert_eq!(env.recorded("AffectedHistoryQuery").len(), 1);
    }

    #[test]
    fn delete_reaction_by_sender_bands_follow_the_cpp() {
        let mut manager = MessageQueryManager::new();
        let mut env = RecordingEnv::default();

        // the sender must be known (the input-peer gate)
        let (promise, cell) = record::<()>();
        manager.delete_reaction_by_sender(
            &mut env,
            user_dialog(777),
            crate::messages_manager::MessageId::new(2000001),
            DialogId::from_user_id(crate::user_manager::UserId::new(42)),
            promise,
        );
        assert_eq!(
            recorded_error(&cell),
            (400, "Reaction sender not found".to_owned())
        );

        // a local message id is rejected after the gate opens
        env.input_peer = true;
        let (promise, cell) = record::<()>();
        manager.delete_reaction_by_sender(
            &mut env,
            user_dialog(777),
            crate::messages_manager::MessageId::new(1),
            DialogId::from_user_id(crate::user_manager::UserId::new(42)),
            promise,
        );
        assert_eq!(
            recorded_error(&cell),
            (400, "Invalid message identifier specified".to_owned())
        );
    }

    #[test]
    fn get_personal_chat_history_bands_follow_the_cpp() {
        let mut manager = MessageQueryManager::new();
        let mut env = RecordingEnv::default();

        // the input-user probe fails first
        let (promise, cell) = record::<td_api::Messages>();
        manager.get_personal_chat_history(
            &mut env,
            crate::user_manager::UserId::new(42),
            5,
            promise,
        );
        assert_eq!(recorded_error(&cell), (400, "User not found".to_owned()));

        // the limit band follows the C++ text
        env.input_user_ok = true;
        let (promise, cell) = record::<td_api::Messages>();
        manager.get_personal_chat_history(
            &mut env,
            crate::user_manager::UserId::new(42),
            0,
            promise,
        );
        assert_eq!(
            recorded_error(&cell),
            (400, "Limit must be positive".to_owned())
        );

        // then the history query parks
        let (promise, _) = record::<td_api::Messages>();
        manager.get_personal_chat_history(
            &mut env,
            crate::user_manager::UserId::new(42),
            5,
            promise,
        );
        assert_eq!(env.recorded("GetPersonalChannelHistoryQuery").len(), 1);
    }

    #[test]
    fn report_music_listen_bands_follow_the_cpp() {
        let mut manager = MessageQueryManager::new();
        let mut env = RecordingEnv::default();

        // negative durations are rejected
        let (promise, cell) = record::<()>();
        manager.report_music_listen(&mut env, crate::files::FileId::new(5, 0), -1, promise);
        assert_eq!(
            recorded_error(&cell),
            (400, "Invalid duration specified".to_owned())
        );

        // invalid file identifiers are rejected
        let (promise, cell) = record::<()>();
        manager.report_music_listen(&mut env, crate::files::FileId::new(0, 0), 10, promise);
        assert_eq!(
            recorded_error(&cell),
            (400, "Invalid file identifier specified".to_owned())
        );

        // short listens and the missing-document band answer silently
        let (promise, cell) = record::<()>();
        manager.report_music_listen(&mut env, crate::files::FileId::new(5, 0), 1, promise);
        assert!(cell.lock().unwrap().is_some());
        assert!(env.recorded("ReportMusicListenQuery").is_empty());

        // a reportable document rides the query tail
        env.music_listen_document = Some(telegram_api::InputDocument::inputDocument(
            telegram_api::inputDocument {
                id: 5,
                access_hash: 0,
                file_reference: Vec::new(),
            },
        ));
        let (promise, _) = record::<()>();
        manager.report_music_listen(&mut env, crate::files::FileId::new(5, 0), 10, promise);
        assert_eq!(env.recorded("ReportMusicListenQuery").len(), 1);
    }

    #[test]
    fn share_dialogs_with_bot_bands_follow_the_cpp() {
        let mut manager = MessageQueryManager::new();
        let mut env = RecordingEnv::default();

        // the source must be non-empty
        let (promise, cell) = record::<()>();
        manager.share_dialogs_with_bot(&mut env, None, 1, Vec::new(), true, false, promise);
        assert_eq!(
            recorded_error(&cell),
            (400, "Source must be non-empty".to_owned())
        );

        // the message-source leg needs the message plane
        let (promise, cell) = record::<()>();
        manager.share_dialogs_with_bot(
            &mut env,
            Some(td_api::KeyboardButtonSource::keyboardButtonSourceMessage(
                td_api::keyboardButtonSourceMessage {
                    chat_id: -10020000000001,
                    message_id: 2000001,
                },
            )),
            1,
            Vec::new(),
            true,
            false,
            promise,
        );
        assert_eq!(recorded_error(&cell), (400, "Message not found".to_owned()));

        // the web-app-source leg needs the prepared-button store
        let (promise, cell) = record::<()>();
        manager.share_dialogs_with_bot(
            &mut env,
            Some(td_api::KeyboardButtonSource::keyboardButtonSourceWebApp(
                td_api::keyboardButtonSourceWebApp {
                    bot_user_id: 42,
                    prepared_button_id: "button".to_owned(),
                },
            )),
            1,
            Vec::new(),
            true,
            false,
            promise,
        );
        assert_eq!(recorded_error(&cell), (400, "Button not found".to_owned()));
    }

    #[test]
    fn the_message_search_filter_table_follows_the_cpp() {
        use crate::messages_manager::MessageSearchFilter as F;
        assert_eq!(get_message_search_filter(None), F::Empty);
        assert_eq!(
            get_message_search_filter(Some(
                &td_api::SearchMessagesFilter::searchMessagesFilterAnimation(
                    td_api::searchMessagesFilterAnimation {}
                )
            )),
            F::Animation
        );
        assert_eq!(
            get_message_search_filter(Some(
                &td_api::SearchMessagesFilter::searchMessagesFilterVoiceAndVideoNote(
                    td_api::searchMessagesFilterVoiceAndVideoNote {}
                )
            )),
            F::VoiceAndVideoNote
        );
        assert_eq!(
            get_message_search_filter(Some(
                &td_api::SearchMessagesFilter::searchMessagesFilterUnreadPollVote(
                    td_api::searchMessagesFilterUnreadPollVote {}
                )
            )),
            F::UnreadPollVote
        );
    }
}
