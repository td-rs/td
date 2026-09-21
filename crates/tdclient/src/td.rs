//! Port of `td/telegram/Td.{h,cpp}` (Track I, Td M.1): the full request
//! state machine — `WaitParameters → Run → Close` — the `setTdlibParameters`
//! validation (`Td::get_parameters`), the in-process `TdDb::open` + init
//! sequence (options pmc replay, AuthManager restore), the request tracking
//! (`request_set_`), the `send_result`/`send_error` semantics and the
//! close/destroy machine.
//!
//! S76 adds the walking-skeleton live path: the [`NetBridge`](crate::net)
//! session (spawned by `init`, updates flowing back through `OnNetUpdate`),
//! the authorization requests over the bridge (phone → code → 2FA → Ready,
//! or the persisted-state resume), `getMe`, the Saved-Messages text
//! `sendMessage` (the temporary-message flow) and the update receive path
//! (`updateUser`/`updateNewChat`/`updateChatReadInbox`/`updateNewMessage`/
//! `updateChatLastMessage`).
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
//! - the S76 bridge is one dedicated thread with a blocking query API (the
//!   C++ runs the net stack as actors; the observable JSON behavior is
//!   identical for the single-client skeleton).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tdactor::{Actor, ActorOwn, Scheduler, actor_messages, send_closure_later};
use tddb::td_db::{OpenedDatabase, Parameters as DbParameters, TdDb};
use tdmtproto::telegram_api;
use tdnet::net_query::{NetQuery, NetQueryDispatcher};
use tdutils::promise::Promise;
use tdutils::status::Status;

use crate::auth_manager::{AuthManager, AuthQuerySender};
use crate::chat_manager::{ChannelId, ChatId, MessageId, RestrictedRights};
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
use crate::contact::Contact;
use crate::convert::{self, MessageSpec};
use crate::dialog_manager::DialogManager;
use crate::emoji_status::CustomEmojiId;
use crate::global::Global;
use crate::link_manager::LinkManager;
use crate::manager_option_states::ManagerOptionStates;
use crate::message_content::*;
use crate::message_entity::FormattedText;
use crate::messages_manager::{
    DialogId, EphemeralMessageFullId, FileUploadId, MessageFullId, QuickReplyMessageFullId,
    StoryFullId, StoryId,
};
use crate::net::{AuthSender, BridgeConfig, NetBridge, QueryAnswer};
use crate::notification_manager::NotificationManager;
use crate::option_manager::OptionManager;
use crate::photo::{Dimensions, Photo, PhotoSize};
use crate::query_merger::QueryMerger;
use crate::requests;
use crate::synchronous_requests;
use crate::td_api::{self, Function, Object};
use crate::time_zone_manager::TimeZoneManager;
use crate::updates_manager::{PendingUpdate, UpdatesManager, UpdatesManagerEnv, UpdatesSink};
use crate::user_manager::FileId;
use crate::user_manager::SecretChatId;
use crate::user_manager::{UserEnv, UserId, UserManager};

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

/// `Td::Parameters` — the non-database initialization parameters (S76 adds
/// the bridge-facing half).
pub struct Parameters {
    pub api_id: i32,
    pub api_hash: String,
    pub use_secret_chats: bool,
    pub use_test_dc: bool,
    pub database_directory: PathBuf,
}

/// `MtprotoHeader::Options` — gathered by `get_parameters`/`init`; consumed
/// by the bridge's `initConnection` wrap.
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

/// A `sendMessage` in flight: the temporary local message exists, the
/// answer was already sent, and the wire ack completes it.
struct PendingSend {
    local_id: i64,
    chat_id: i64,
    text: String,
}

/// The td-level [`UpdatesSink`] — the collector form carrying the M.10
/// receive path: the engine drives it (the users/chats pre-application
/// and the applied pts updates) and `Td::drain_sink` converts/emits
/// through the landed managers. The direct manager calls land with the
/// sink-state refactor; the drain pattern keeps the M.10.3/M.10.6
/// intakes live.
#[derive(Default)]
pub(crate) struct TdSink {
    pub(crate) users: Vec<tdmtproto::telegram_api::User>,
    pub(crate) chats: Vec<tdmtproto::telegram_api::Chat>,
    pub(crate) new_messages: Vec<(tdmtproto::telegram_api::Message, &'static str)>,
    pub(crate) deletions: Vec<Vec<i32>>,
}

impl UpdatesSink for TdSink {
    fn on_user(&mut self, user: tdmtproto::telegram_api::User) {
        self.users.push(user);
    }
    fn on_chat(&mut self, chat: tdmtproto::telegram_api::Chat) {
        self.chats.push(chat);
    }
    /// `MessagesManager::process_pts_update` — the applied update switch:
    /// the new-message and delete kinds collect for the drain (the
    /// M.10.3/M.10.6 intakes); the rest are the manager flips.
    fn process_pts_update(&mut self, update: PendingUpdate) {
        match update {
            PendingUpdate::Wire(telegram_api::Update::updateNewMessage(update)) => {
                if let Some(message) = update.message {
                    self.new_messages.push((message, "updateNewMessage"));
                }
            }
            PendingUpdate::Wire(telegram_api::Update::updateNewChannelMessage(update)) => {
                if let Some(message) = update.message {
                    self.new_messages.push((message, "updateNewChannelMessage"));
                }
            }
            PendingUpdate::Wire(telegram_api::Update::updateDeleteMessages(update)) => {
                self.deletions.push(update.messages);
            }
            _ => {}
        }
    }
    /// `MessagesManager::add_pending_channel_update` — the channel-pts
    /// machine's applied updates ride the same collector.
    fn add_pending_channel_update(
        &mut self,
        _peer: Option<tdmtproto::telegram_api::Peer>,
        update: PendingUpdate,
        _pts: i32,
        _pts_count: i32,
        _promise: Promise<()>,
        _source: &str,
    ) {
        self.process_pts_update(update);
    }
}

/// The Td-side [`crate::message_content::MessageContentContext`] adapter —
/// the M.9.18 receive flip. The real reads are the Td-owned surfaces
/// (auth, options, ids, unix time); the manager surfaces answer the
/// documented stand-ins until their owners land.
impl crate::message_content::MessageContentContext for Td {
    fn get_user_default_permissions(&self, _user_id: UserId) -> RestrictedRights {
        RestrictedRights::default()
    }
    fn get_secret_chat_default_permissions(
        &self,
        _secret_chat_id: SecretChatId,
    ) -> RestrictedRights {
        RestrictedRights::default()
    }
    fn is_user_bot(&self, user_id: UserId) -> bool {
        self.users
            .get(&user_id.get())
            .is_some_and(|u| matches!(u.r#type, Some(td_api::UserType::userTypeBot(_))))
    }
    fn get_user_voice_messages_forbidden(&self, _user_id: UserId) -> bool {
        false
    }
    fn get_chat_effective_restricted_rights(&self, _chat_id: ChatId) -> RestrictedRights {
        RestrictedRights::default()
    }
    fn get_channel_effective_restricted_rights(&self, _channel_id: ChannelId) -> RestrictedRights {
        RestrictedRights::default()
    }
    fn is_broadcast_channel(&self, _channel_id: ChannelId) -> bool {
        false
    }
    fn is_monoforum_channel(&self, _channel_id: ChannelId) -> bool {
        false
    }
    fn is_bot(&self) -> bool {
        self.auth_manager.is_bot()
    }
    fn get_my_dialog_id(&self) -> DialogId {
        crate::messages_manager::DialogId::from_user_id(crate::user_manager::UserId::new(
            self.my_id,
        ))
    }
    fn get_poll_is_anonymous(&self, _poll_id: PollId) -> bool {
        false
    }
    fn get_poll_is_closed(&self, _poll_id: PollId) -> bool {
        false
    }
    fn get_poll_can_add_option(&self, _poll_id: PollId) -> bool {
        false
    }
    fn get_poll_can_view_stats(&self, _poll_id: PollId) -> bool {
        false
    }
    fn get_poll_has_unread_votes(&self, _poll_id: PollId) -> bool {
        false
    }
    fn remove_poll_has_unread_votes(&self, _poll_id: PollId) {
        {}
    }
    fn has_input_media_poll(&self, _poll_id: PollId) -> bool {
        false
    }
    fn get_poll_individual_message_contents(
        &self,
        _poll_id: PollId,
        _attached_media: Option<&MessageContent>,
    ) -> Vec<MessageContent> {
        Vec::new()
    }
    fn get_poll_option_properties(
        &self,
        _poll_id: PollId,
        _option_id: &str,
        _dialog_id: DialogId,
        _message_id: MessageId,
        _can_be_replied: bool,
        _can_be_replied_in_another_chat: bool,
        _can_get_link: bool,
        _is_forward: bool,
        _is_outgoing: bool,
        _promise: Promise<crate::td_api::PollOptionProperties>,
    ) {
        {}
    }
    fn get_sticker_type(&self, _file_id: FileId) -> StickerType {
        StickerType::default()
    }
    fn get_web_page_user_ids(&self, _web_page_id: WebPageId) -> Vec<UserId> {
        Vec::new()
    }
    fn get_web_page_channel_ids(&self, _web_page_id: WebPageId) -> Vec<ChannelId> {
        Vec::new()
    }
    fn get_option_integer(&self, name: &str, default: i64) -> i64 {
        self.option_manager
            .as_ref()
            .map_or(default, |om| om.get_option_integer(name, default))
    }
    fn register_web_page(
        &mut self,
        _web_page_id: WebPageId,
        _message_full_id: MessageFullId,
        _source: &str,
    ) {
        {}
    }
    fn unregister_web_page(
        &mut self,
        _web_page_id: WebPageId,
        _message_full_id: MessageFullId,
        _source: &str,
    ) {
        {}
    }
    fn register_quick_reply_web_page(
        &mut self,
        _web_page_id: WebPageId,
        _message_full_id: QuickReplyMessageFullId,
        _source: &str,
    ) {
        {}
    }
    fn unregister_quick_reply_web_page(
        &mut self,
        _web_page_id: WebPageId,
        _message_full_id: QuickReplyMessageFullId,
        _source: &str,
    ) {
        {}
    }
    fn register_welcome_message_web_page(
        &mut self,
        _web_page_id: WebPageId,
        _message_full_id: EphemeralMessageFullId,
        _source: &str,
    ) {
        {}
    }
    fn unregister_welcome_message_web_page(
        &mut self,
        _web_page_id: WebPageId,
        _message_full_id: EphemeralMessageFullId,
        _source: &str,
    ) {
        {}
    }
    fn register_premium_gift(
        &mut self,
        _months: i32,
        _star_count: i64,
        _message_full_id: MessageFullId,
        _source: &str,
    ) {
        {}
    }
    fn unregister_premium_gift(
        &mut self,
        _months: i32,
        _star_count: i64,
        _message_full_id: MessageFullId,
        _source: &str,
    ) {
        {}
    }
    fn register_ton_gift(
        &mut self,
        _ton_count: i64,
        _message_full_id: MessageFullId,
        _source: &str,
    ) {
        {}
    }
    fn unregister_ton_gift(
        &mut self,
        _ton_count: i64,
        _message_full_id: MessageFullId,
        _source: &str,
    ) {
        {}
    }
    fn register_dice(
        &mut self,
        _emoji: &str,
        _value: i32,
        _message_full_id: MessageFullId,
        _quick_reply_message_full_id: QuickReplyMessageFullId,
        _source: &str,
    ) {
        {}
    }
    fn unregister_dice(
        &mut self,
        _emoji: &str,
        _value: i32,
        _message_full_id: MessageFullId,
        _quick_reply_message_full_id: QuickReplyMessageFullId,
        _source: &str,
    ) {
        {}
    }
    fn register_emoji(
        &mut self,
        _emoji: &str,
        _custom_emoji_id: CustomEmojiId,
        _message_full_id: MessageFullId,
        _quick_reply_message_full_id: QuickReplyMessageFullId,
        _ephemeral_message_full_id: EphemeralMessageFullId,
        _source: &str,
    ) {
        {}
    }
    fn unregister_emoji(
        &mut self,
        _emoji: &str,
        _custom_emoji_id: CustomEmojiId,
        _message_full_id: MessageFullId,
        _quick_reply_message_full_id: QuickReplyMessageFullId,
        _ephemeral_message_full_id: EphemeralMessageFullId,
        _source: &str,
    ) {
        {}
    }
    fn register_voice(
        &mut self,
        _file_id: FileId,
        _content_type: MessageContentType,
        _message_full_id: MessageFullId,
        _source: &str,
    ) {
        {}
    }
    fn unregister_voice(
        &mut self,
        _file_id: FileId,
        _content_type: MessageContentType,
        _message_full_id: MessageFullId,
        _source: &str,
    ) {
        {}
    }
    fn register_poll(&mut self, _poll_id: PollId, _message_full_id: MessageFullId, _source: &str) {
        {}
    }
    fn unregister_poll(
        &mut self,
        _poll_id: PollId,
        _message_full_id: MessageFullId,
        _source: &str,
    ) {
        {}
    }
    fn register_reply_poll(&mut self, _poll_id: PollId) {
        {}
    }
    fn unregister_reply_poll(&mut self, _poll_id: PollId) {
        {}
    }
    fn register_story(
        &mut self,
        _story_full_id: StoryFullId,
        _message_full_id: MessageFullId,
        _quick_reply_message_full_id: QuickReplyMessageFullId,
        _source: &str,
    ) {
        {}
    }
    fn unregister_story(
        &mut self,
        _story_full_id: StoryFullId,
        _message_full_id: MessageFullId,
        _quick_reply_message_full_id: QuickReplyMessageFullId,
        _source: &str,
    ) {
        {}
    }
    fn register_group_call(&mut self, _message_full_id: MessageFullId, _source: &str) {
        {}
    }
    fn unregister_group_call(&mut self, _message_full_id: MessageFullId, _source: &str) {
        {}
    }
    fn register_suggested_profile_photo(&mut self, _photo: &Photo) {
        {}
    }
    fn register_noforwards_request(&mut self, _message_full_id: MessageFullId, _message_date: i32) {
        {}
    }
    fn unregister_noforwards_request(&mut self, _message_full_id: MessageFullId) {
        {}
    }
    fn merge_animations(&mut self, _new_file_id: FileId, _old_file_id: FileId) {
        {}
    }
    fn merge_audios(&mut self, _new_file_id: FileId, _old_file_id: FileId) {
        {}
    }
    fn merge_documents(&mut self, _new_file_id: FileId, _old_file_id: FileId) {
        {}
    }
    fn merge_stickers(&mut self, _new_file_id: FileId, _old_file_id: FileId) {
        {}
    }
    fn merge_videos(&mut self, _new_file_id: FileId, _old_file_id: FileId) {
        {}
    }
    fn merge_video_notes(&mut self, _new_file_id: FileId, _old_file_id: FileId) {
        {}
    }
    fn merge_voice_notes(&mut self, _new_file_id: FileId, _old_file_id: FileId) {
        {}
    }
    fn merge_files(&mut self, _new_file_id: FileId, _old_file_id: FileId) {
        {}
    }
    fn merge_photo_files(&mut self, _old_photo: &Photo, _new_photo: &Photo, _dialog_id: DialogId) {
        {}
    }
    fn have_web_page(&self, _web_page_id: WebPageId) -> bool {
        false
    }
    fn try_merge_documents(&mut self, _new_file_id: FileId, _old_file_id: FileId) {
        {}
    }
    fn get_option_boolean(&self, name: &str, default_value: bool) -> bool {
        self.option_manager
            .as_ref()
            .is_some_and(|om| om.get_option_boolean(name, default_value))
    }
    fn get_secret_chat_layer(&self, _secret_chat_id: SecretChatId) -> i32 {
        0
    }
    fn get_secret_chat_user_id(&self, _secret_chat_id: SecretChatId) -> UserId {
        UserId::default()
    }
    fn can_use_premium_custom_emoji_in_dialog(&self, _dialog_id: DialogId) -> bool {
        false
    }
    fn is_premium_custom_emoji(&self, _custom_emoji_id: CustomEmojiId) -> bool {
        false
    }
    fn get_channel_can_add_web_page_previews(&self, _channel_id: ChannelId) -> bool {
        false
    }
    fn get_web_page_by_url(&self, _url: &str) -> WebPageId {
        WebPageId::default()
    }
    fn have_user_force(&self, _user_id: UserId, _source: &str) -> bool {
        false
    }
    fn ensure_input_user(&self, _user_id: UserId) -> Result<(), Status> {
        unreachable!("send-path seam reached on the receive flip")
    }
    fn get_my_id(&self) -> UserId {
        crate::user_manager::UserId::new(self.my_id)
    }
    fn get_file_view_details(&self, _file_id: FileId) -> Option<(String, String, String)> {
        None
    }
    fn create_animation(
        &mut self,
        _file_id: FileId,
        _thumbnail: PhotoSize,
        _has_stickers: bool,
        _sticker_file_ids: Vec<FileId>,
        _file_name: String,
        _mime_type: String,
        _duration: i32,
        _dimensions: Dimensions,
    ) {
        {}
    }
    fn create_audio(
        &mut self,
        _file_id: FileId,
        _thumbnail: PhotoSize,
        _file_name: String,
        _mime_type: String,
        _duration: i32,
        _title: String,
        _performer: String,
    ) {
        {}
    }
    fn create_document(
        &mut self,
        _file_id: FileId,
        _thumbnail: PhotoSize,
        _file_name: String,
        _mime_type: String,
    ) {
        {}
    }
    fn create_sticker(&mut self, _file_id: FileId, _thumbnail: PhotoSize, _dimensions: Dimensions) {
        {}
    }
    fn create_video(
        &mut self,
        _file_id: FileId,
        _thumbnail: PhotoSize,
        _has_stickers: bool,
        _sticker_file_ids: Vec<FileId>,
        _file_name: String,
        _mime_type: String,
        _duration: i32,
        _dimensions: Dimensions,
        _supports_streaming: bool,
    ) {
        {}
    }
    fn create_video_note(
        &mut self,
        _file_id: FileId,
        _thumbnail: PhotoSize,
        _duration: i32,
        _dimensions: Dimensions,
    ) {
        {}
    }
    fn create_voice_note(
        &mut self,
        _file_id: FileId,
        _mime_type: String,
        _duration: i32,
        _waveform: Vec<u8>,
    ) {
        {}
    }
    fn create_photo_input(
        &mut self,
        _file_id: FileId,
        _thumbnail: PhotoSize,
        _width: i32,
        _height: i32,
        _sticker_file_ids: Vec<FileId>,
    ) -> Result<Photo, Status> {
        unreachable!("send-path seam reached on the receive flip")
    }
    fn get_input_file_id(
        &mut self,
        _file_type: crate::files::FileType,
        _input_file: Option<td_api::InputFile>,
        _dialog_id: DialogId,
        _allow_get_by_hash: bool,
        _is_secret: bool,
    ) -> Result<FileId, Status> {
        unreachable!("send-path seam reached on the receive flip")
    }
    fn get_attached_sticker_file_ids(&self, _sticker_file_ids: Vec<i32>) -> Vec<FileId> {
        Vec::new()
    }
    fn get_input_thumbnail_photo_size(
        &mut self,
        _thumbnail: Option<td_api::InputThumbnail>,
        _dialog_id: DialogId,
        _is_secret: bool,
    ) -> Result<PhotoSize, Status> {
        unreachable!("send-path seam reached on the receive flip")
    }
    fn from_persistent_id(
        &self,
        _persistent_id: &str,
        _file_type: crate::files::FileType,
    ) -> Result<FileId, Status> {
        unreachable!("send-path seam reached on the receive flip")
    }
    fn is_broadcast_channel_dialog(&self, _dialog_id: DialogId) -> bool {
        false
    }
    fn create_poll(
        &mut self,
        _question: FormattedText,
        _options: Vec<PollOption>,
        _is_anonymous: bool,
        _allows_multiple_answers: bool,
        _has_open_answers: bool,
        _fixed_answers: bool,
        _members_only: bool,
        _country_codes: Vec<String>,
        _shuffle_options: bool,
        _hide_results_until_closes: bool,
        _is_quiz: bool,
        _correct_option_ids: Vec<i32>,
        _explanation: FormattedText,
        _explanation_media: Option<Box<MessageContent>>,
        _open_period: i32,
        _close_date: i32,
        _is_closed: bool,
    ) -> PollId {
        PollId::default()
    }
    fn have_story_force(&self, _story_full_id: StoryFullId) -> bool {
        false
    }
    fn has_read_access_input_peer(&self, _dialog_id: DialogId) -> bool {
        false
    }
    fn get_input_user_force(&self, _user_id: UserId) -> Option<telegram_api::InputUser> {
        None
    }
    fn get_animation_input_media(
        &self,
        _file_id: FileId,
        _input_file: Option<telegram_api::InputFile>,
        _input_thumbnail: Option<telegram_api::InputFile>,
        _has_spoiler: bool,
    ) -> Option<telegram_api::InputMedia> {
        None
    }
    fn get_audio_input_media(
        &self,
        _file_id: FileId,
        _input_file: Option<telegram_api::InputFile>,
        _input_thumbnail: Option<telegram_api::InputFile>,
    ) -> Option<telegram_api::InputMedia> {
        None
    }
    fn get_document_input_media(
        &self,
        _file_id: FileId,
        _input_file: Option<telegram_api::InputFile>,
        _input_thumbnail: Option<telegram_api::InputFile>,
    ) -> Option<telegram_api::InputMedia> {
        None
    }
    fn get_sticker_input_media(
        &self,
        _file_id: FileId,
        _input_file: Option<telegram_api::InputFile>,
        _input_thumbnail: Option<telegram_api::InputFile>,
        _emoji: &str,
    ) -> Option<telegram_api::InputMedia> {
        None
    }
    fn get_video_input_media(
        &self,
        _file_id: FileId,
        _input_file: Option<telegram_api::InputFile>,
        _input_thumbnail: Option<telegram_api::InputFile>,
        _cover: &Photo,
        _start_timestamp: i32,
        _ttl: i32,
        _has_spoiler: bool,
    ) -> Option<telegram_api::InputMedia> {
        None
    }
    fn get_video_note_input_media(
        &self,
        _file_id: FileId,
        _input_file: Option<telegram_api::InputFile>,
        _input_thumbnail: Option<telegram_api::InputFile>,
        _ttl: i32,
    ) -> Option<telegram_api::InputMedia> {
        None
    }
    fn get_voice_note_input_media(
        &self,
        _file_id: FileId,
        _input_file: Option<telegram_api::InputFile>,
        _ttl: i32,
    ) -> Option<telegram_api::InputMedia> {
        None
    }
    fn get_poll_input_media(
        &self,
        _poll_id: PollId,
        _input_media: Vec<Option<telegram_api::InputMedia>>,
    ) -> Option<telegram_api::InputMedia> {
        None
    }
    fn get_story_input_media(
        &self,
        _story_full_id: StoryFullId,
    ) -> Option<telegram_api::InputMedia> {
        None
    }
    fn photo_get_input_media(
        &self,
        _photo: &Photo,
        _input_file: Option<telegram_api::InputFile>,
        _ttl: i32,
        _has_spoiler: bool,
        _video_file_id: FileId,
    ) -> Option<telegram_api::InputMedia> {
        None
    }
    fn get_animation_secret_input_media(
        &self,
        _file_id: FileId,
        _input_file: Option<telegram_api::InputEncryptedFile>,
        _caption: &FormattedText,
        _thumbnail: Vec<u8>,
        _layer: i32,
    ) -> SecretInputMedia {
        SecretInputMedia::default()
    }
    fn get_audio_secret_input_media(
        &self,
        _file_id: FileId,
        _input_file: Option<telegram_api::InputEncryptedFile>,
        _caption: &FormattedText,
        _thumbnail: Vec<u8>,
        _layer: i32,
    ) -> SecretInputMedia {
        SecretInputMedia::default()
    }
    fn get_document_secret_input_media(
        &self,
        _file_id: FileId,
        _input_file: Option<telegram_api::InputEncryptedFile>,
        _caption: &FormattedText,
        _thumbnail: Vec<u8>,
        _layer: i32,
    ) -> SecretInputMedia {
        SecretInputMedia::default()
    }
    fn get_sticker_secret_input_media(
        &self,
        _file_id: FileId,
        _input_file: Option<telegram_api::InputEncryptedFile>,
        _thumbnail: Vec<u8>,
        _layer: i32,
    ) -> SecretInputMedia {
        SecretInputMedia::default()
    }
    fn get_video_secret_input_media(
        &self,
        _file_id: FileId,
        _input_file: Option<telegram_api::InputEncryptedFile>,
        _caption: &FormattedText,
        _thumbnail: Vec<u8>,
        _layer: i32,
    ) -> SecretInputMedia {
        SecretInputMedia::default()
    }
    fn get_video_note_secret_input_media(
        &self,
        _file_id: FileId,
        _input_file: Option<telegram_api::InputEncryptedFile>,
        _thumbnail: Vec<u8>,
        _layer: i32,
    ) -> SecretInputMedia {
        SecretInputMedia::default()
    }
    fn get_voice_note_secret_input_media(
        &self,
        _file_id: FileId,
        _input_file: Option<telegram_api::InputEncryptedFile>,
        _caption: &FormattedText,
        _layer: i32,
    ) -> SecretInputMedia {
        SecretInputMedia::default()
    }
    fn photo_get_secret_input_media(
        &self,
        _photo: &Photo,
        _input_file: Option<telegram_api::InputEncryptedFile>,
        _caption: &FormattedText,
        _thumbnail: Vec<u8>,
    ) -> SecretInputMedia {
        SecretInputMedia::default()
    }
    fn get_input_web_document(&self, _photo: &Photo) -> Option<telegram_api::InputWebDocument> {
        None
    }
    fn delete_animation_thumbnail(&mut self, _file_id: FileId) {
        {}
    }
    fn delete_audio_thumbnail(&mut self, _file_id: FileId) {
        {}
    }
    fn delete_document_thumbnail(&mut self, _file_id: FileId) {
        {}
    }
    fn delete_sticker_thumbnail(&mut self, _file_id: FileId) {
        {}
    }
    fn delete_video_thumbnail(&mut self, _file_id: FileId) {
        {}
    }
    fn delete_video_note_thumbnail(&mut self, _file_id: FileId) {
        {}
    }
    fn cancel_upload(&mut self, _file_upload_id: FileUploadId) {
        {}
    }
    fn was_authorized(&self) -> bool {
        self.auth_manager.is_authorized()
    }
    fn get_live_photo_video_file_id(
        &mut self,
        _video: Option<telegram_api::Document>,
        _owner_dialog_id: DialogId,
        _is_self_destructing: bool,
    ) -> FileId {
        FileId::default()
    }
    fn on_get_document(
        &mut self,
        _document: telegram_api::Document,
        _owner_dialog_id: DialogId,
        _is_self_destructing: bool,
        _is_send_priority: bool,
        _doc_type: Option<DocumentType>,
    ) -> ParsedDocument {
        ParsedDocument::default()
    }
    fn get_web_document_photo(
        &mut self,
        _web_document: Option<telegram_api::WebDocument>,
        _owner_dialog_id: DialogId,
    ) -> Photo {
        Photo::default()
    }
    fn on_get_web_page(
        &mut self,
        _web_page: Option<telegram_api::WebPage>,
        _owner_dialog_id: DialogId,
    ) -> WebPageId {
        WebPageId::default()
    }
    fn on_get_poll(
        &mut self,
        _poll: Option<telegram_api::Poll>,
        _results: Option<telegram_api::PollResults>,
        _source: &str,
    ) -> PollId {
        PollId::default()
    }
    fn on_get_story(
        &mut self,
        _dialog_id: DialogId,
        _story: Option<telegram_api::StoryItem>,
    ) -> StoryId {
        StoryId::default()
    }
    fn force_create_dialog(&mut self, _dialog_id: DialogId, _source: &str) {
        {}
    }
    fn is_file_encrypted_secret(&self, _file_id: FileId) -> bool {
        false
    }
    fn copy_file_id_to_secret(&mut self, _file_id: FileId, _dialog_id: DialogId) -> FileId {
        FileId::default()
    }
    fn dup_animation(&mut self, _new_file_id: FileId, _old_file_id: FileId) -> FileId {
        FileId::default()
    }
    fn dup_audio(&mut self, _new_file_id: FileId, _old_file_id: FileId) -> FileId {
        FileId::default()
    }
    fn dup_document(&mut self, _new_file_id: FileId, _old_file_id: FileId) -> FileId {
        FileId::default()
    }
    fn dup_sticker(&mut self, _new_file_id: FileId, _old_file_id: FileId) -> FileId {
        FileId::default()
    }
    fn dup_video(&mut self, _new_file_id: FileId, _old_file_id: FileId) -> FileId {
        FileId::default()
    }
    fn dup_video_note(&mut self, _new_file_id: FileId, _old_file_id: FileId) -> FileId {
        FileId::default()
    }
    fn dup_voice_note(&mut self, _new_file_id: FileId, _old_file_id: FileId) -> FileId {
        FileId::default()
    }
    fn dup_poll(&mut self, _dialog_id: DialogId, _poll_id: PollId) -> PollId {
        PollId::default()
    }
    fn has_secret_input_media_sticker(&self, _file_id: FileId) -> bool {
        false
    }
    fn is_user_deleted(&self, _user_id: UserId) -> bool {
        false
    }
    fn get_recent_call_access_hash(&self, _call_id: i64) -> i64 {
        0
    }
    fn get_encrypted_secure_values(
        &mut self,
        _secure_values: Vec<telegram_api::SecureValue>,
    ) -> Vec<EncryptedSecureValue> {
        Vec::new()
    }
    fn get_shared_dialog_from_peer(
        &mut self,
        _peer: telegram_api::RequestedPeer,
    ) -> Option<SharedDialog> {
        None
    }
    fn get_background_info(
        &mut self,
        _wallpaper: Option<telegram_api::WallPaper>,
        _is_local: bool,
        _for_dark_theme: bool,
    ) -> Option<BackgroundInfo> {
        None
    }
    fn get_web_app_from_bot_app(
        &mut self,
        _bot_app: telegram_api::BotApp,
        _owner_dialog_id: DialogId,
    ) -> WebApp {
        WebApp::default()
    }
    fn parse_poll_option_answer(
        &mut self,
        _answer: Option<telegram_api::PollAnswer>,
    ) -> Option<(FormattedText, String)> {
        None
    }
    fn unix_time(&self) -> i32 {
        self.global.unix_time()
    }
    fn get_user_id_object(&self, user_id: UserId, _source: &str) -> i64 {
        user_id.get()
    }
    fn get_user_ids_object(&self, user_ids: &[UserId], _source: &str) -> Vec<i64> {
        user_ids.iter().map(|&u| u.get()).collect()
    }
    fn is_user_support(&self, _user_id: UserId) -> bool {
        false
    }
    fn get_service_notifications_user_id(&self) -> UserId {
        crate::user_manager::UserId::new(777000)
    }
    fn get_supergroup_id_object(&self, channel_id: ChannelId, _source: &str) -> i64 {
        channel_id.get()
    }
    fn get_basic_group_id_object(&self, chat_id: ChatId, _source: &str) -> i64 {
        chat_id.get()
    }
    fn get_chat_id_object(&self, dialog_id: DialogId, _source: &str) -> i64 {
        match dialog_id.get_type() {
            crate::messages_manager::DialogType::User => dialog_id.get_user_id().get(),
            crate::messages_manager::DialogType::Chat => -dialog_id.get_chat_id().get(),
            crate::messages_manager::DialogType::Channel => dialog_id.get_channel_id().get(),
            crate::messages_manager::DialogType::SecretChat => {
                i64::from(dialog_id.get_secret_chat_id().get())
            }
            crate::messages_manager::DialogType::None => 0,
        }
    }
    fn have_input_peer(&self, _dialog_id: DialogId) -> bool {
        true
    }
    fn get_group_call_id(
        &self,
        _input_group_call_id: InputGroupCallId,
        _dialog_id: DialogId,
    ) -> i64 {
        0
    }
    fn get_message_sender_object(
        &self,
        _dialog_id: DialogId,
        _source: &str,
    ) -> Option<td_api::MessageSender> {
        None
    }
    fn get_contact_object(&self, _contact: &Contact) -> td_api::Contact {
        td_api::Contact::default()
    }
    fn get_animation_object(&self, _file_id: FileId) -> td_api::Animation {
        td_api::Animation::default()
    }
    fn get_audio_object(&self, _file_id: FileId) -> td_api::Audio {
        td_api::Audio::default()
    }
    fn get_document_object(&self, _file_id: FileId) -> td_api::Document {
        td_api::Document::default()
    }
    fn get_video_object(&self, _file_id: FileId) -> td_api::Video {
        td_api::Video::default()
    }
    fn get_video_note_object(&self, _file_id: FileId) -> td_api::VideoNote {
        td_api::VideoNote::default()
    }
    fn get_voice_note_object(&self, _file_id: FileId) -> td_api::VoiceNote {
        td_api::VoiceNote::default()
    }
    fn get_sticker_object(&self, _file_id: FileId) -> Option<td_api::Sticker> {
        None
    }
    fn get_animated_emoji_object(
        &self,
        _text: &str,
        _custom_emoji_id: CustomEmojiId,
    ) -> Option<td_api::AnimatedEmoji> {
        None
    }
    fn get_dice_stickers_object(&self, _emoji: &str, _value: i32) -> td_api::DiceStickers {
        td_api::DiceStickers::default()
    }
    fn get_dice_success_animation_frame_number(&self, _emoji: &str, _value: i32) -> i32 {
        0
    }
    fn get_premium_gift_sticker_object(
        &self,
        _month_count: i32,
        _star_count: i64,
    ) -> Option<td_api::Sticker> {
        None
    }
    fn get_ton_gift_sticker_object(&self, _ton_count: i64) -> Option<td_api::Sticker> {
        None
    }
    fn get_alternative_video_object(
        &self,
        _file_id: FileId,
        _hls_file_ids: &[FileId],
    ) -> Option<td_api::AlternativeVideo> {
        None
    }
    fn get_video_storyboard_object(
        &self,
        _file_id: FileId,
        _storyboard_map_file_ids: &[FileId],
    ) -> Option<td_api::VideoStoryboard> {
        None
    }
    fn get_link_preview_object(
        &self,
        _web_page_id: WebPageId,
        _force_small_media: bool,
        _force_large_media: bool,
        _skip_confirmation: bool,
        _invert_media: bool,
    ) -> Option<td_api::LinkPreview> {
        None
    }
    fn get_photo_object(&self, _photo: &Photo) -> Option<td_api::Photo> {
        None
    }
    fn get_chat_photo_object(&self, _photo: &Photo) -> Option<td_api::ChatPhoto> {
        None
    }
    fn get_poll_object(&self, _poll_id: PollId, _dialog_id: DialogId) -> td_api::Poll {
        td_api::Poll::default()
    }
    fn get_message_invoice_object(
        &self,
        _input_invoice: &InputInvoice,
        _is_server: bool,
        _skip_bot_commands: bool,
        _max_media_timestamp: i32,
    ) -> td_api::MessageContent {
        td_api::MessageContent::default()
    }
    fn get_game_object(
        &self,
        _game: &Game,
        _is_server: bool,
        _skip_bot_commands: bool,
    ) -> td_api::Game {
        td_api::Game::default()
    }
    fn get_web_app_object(&self, _web_app: &WebApp) -> td_api::WebApp {
        td_api::WebApp::default()
    }
    fn get_paid_media_object(&self, _extended_media: &MessageExtendedMedia) -> td_api::PaidMedia {
        td_api::PaidMedia::default()
    }
    fn get_shared_user_object(&self, _shared_dialog: &SharedDialog) -> td_api::SharedUser {
        td_api::SharedUser::default()
    }
    fn get_shared_chat_object(&self, _shared_dialog: &SharedDialog) -> td_api::SharedChat {
        td_api::SharedChat::default()
    }
    fn get_giveaway_parameters_object(
        &self,
        _parameters: &GiveawayParameters,
    ) -> td_api::GiveawayParameters {
        td_api::GiveawayParameters::default()
    }
    fn get_chat_background_object(
        &self,
        _background_info: &BackgroundInfo,
    ) -> td_api::ChatBackground {
        td_api::ChatBackground::default()
    }
    fn get_checklist_object(
        &self,
        _list: &ToDoList,
        _completions: &[ToDoCompletion],
        _dialog_id: DialogId,
        _message_id: MessageId,
        _is_outgoing: bool,
        _is_forward: bool,
        _is_real_message_content: bool,
    ) -> td_api::Checklist {
        td_api::Checklist::default()
    }
    fn get_encrypted_passport_element_object(
        &self,
        _values: &[EncryptedSecureValue],
    ) -> Vec<td_api::EncryptedPassportElement> {
        Vec::new()
    }
    fn get_community_id_object(
        &self,
        _community_id: CommunityId,
        _source: &str,
    ) -> td_api::CommunityId {
        td_api::CommunityId::default()
    }
    fn get_animation_duration(&self, _file_id: FileId) -> i32 {
        0
    }
    fn get_audio_duration(&self, _file_id: FileId) -> i32 {
        0
    }
    fn get_video_duration(&self, _file_id: FileId) -> i32 {
        0
    }
    fn get_video_note_duration(&self, _file_id: FileId) -> i32 {
        0
    }
    fn get_voice_note_duration(&self, _file_id: FileId) -> i32 {
        0
    }
    fn get_story_duration(&self, _story_full_id: StoryFullId) -> i32 {
        0
    }
    fn get_web_page_media_duration(&self, _web_page_id: WebPageId) -> i32 {
        0
    }
    fn has_video_cover_input_media(&self, _file_id: FileId, _is_bot: bool) -> bool {
        false
    }
    fn has_photo_cover_input_media(&self, _photo: &Photo, _is_bot: bool) -> bool {
        false
    }
    fn update_extended_media_to(
        &mut self,
        _media: &mut MessageExtendedMedia,
        _extended_media: Option<telegram_api::MessageExtendedMedia>,
        _owner_dialog_id: DialogId,
    ) -> bool {
        false
    }
    fn stop_local_poll(&mut self, _poll_id: PollId) {
        {}
    }
    fn get_animated_emoji_click_sticker(
        &self,
        _text: &str,
        _message_full_id: MessageFullId,
        _promise: Promise<td_api::Sticker>,
    ) {
        {}
    }
    fn on_animated_emoji_message_clicked(
        &mut self,
        _emoji: &str,
        _message_full_id: MessageFullId,
        _data: &str,
    ) -> Result<(), Status> {
        unreachable!("send-path seam reached on the receive flip")
    }
    fn get_video_mime_type(&self, _file_id: FileId) -> String {
        String::new()
    }
    fn get_poll_file_ids(&self, _poll_id: PollId) -> Vec<FileId> {
        Vec::new()
    }
    fn get_web_page_story_full_id(&self, _web_page_id: WebPageId) -> StoryFullId {
        StoryFullId::default()
    }
    fn get_web_page_search_text(&self, _web_page_id: WebPageId) -> String {
        String::new()
    }
    fn get_animation_search_text(&self, _file_id: FileId) -> String {
        String::new()
    }
    fn get_audio_search_text(&self, _file_id: FileId) -> String {
        String::new()
    }
    fn get_document_search_text(&self, _file_id: FileId) -> String {
        String::new()
    }
    fn get_video_search_text(&self, _file_id: FileId) -> String {
        String::new()
    }
    fn get_poll_search_text(&self, _poll_id: PollId) -> String {
        String::new()
    }
    fn get_message_from_server(&mut self, _message_full_id: MessageFullId, _source: &str) {
        {}
    }
    fn on_forum_topic_edited(
        &mut self,
        _dialog_id: DialogId,
        _forum_topic_id: i32,
        _edited_data: &ForumTopicEditedData,
    ) {
        {}
    }
    fn on_update_user_noforwards(
        &mut self,
        _user_id: UserId,
        _is_me: bool,
        _new_value: bool,
        _is_contact: bool,
        _has_noforwards: bool,
    ) {
        {}
    }
    fn on_update_user_linked_community_id(&mut self, _user_id: UserId, _community_id: CommunityId) {
        {}
    }
    fn on_update_channel_linked_community_id(
        &mut self,
        _channel_id: ChannelId,
        _community_id: CommunityId,
    ) {
        {}
    }
    fn add_saved_animation_by_id(&mut self, _file_id: FileId) {
        {}
    }
    fn add_recent_sticker_by_id(&mut self, _is_attached: bool, _file_id: FileId) {
        {}
    }
    fn move_sticker_set_to_top_by_sticker_id(&mut self, _file_id: FileId) {
        {}
    }
    fn move_sticker_set_to_top_by_custom_emoji_ids(
        &mut self,
        _custom_emoji_ids: Vec<CustomEmojiId>,
    ) {
        {}
    }
}

/// The M.10.3 receive-flip adapter: `Td` as the intake/builder
/// [`crate::messages_manager::MessageObjectContext`]. The reads the M.9.18
/// surface already owns (users, auth, options, clock) answer for real, the
/// manager-owned reads (`have_dialog`, the ephemeral-id registry) walk the
/// M.10.2c skeleton, and the ChatManager/ForumTopic/NotificationSettings
/// surfaces answer the documented stand-ins until their receive slices
/// land (the same defaults the M.10.4 test context pins).
impl crate::messages_manager::MessageObjectContext for Td {
    fn content_context(&self) -> &dyn crate::message_content::MessageContentContext {
        self
    }

    fn is_bot(&self) -> bool {
        crate::message_content::MessageContentContext::is_bot(self)
    }

    fn now(&self) -> f64 {
        tdutils::time::now()
    }

    fn server_time(&self) -> f64 {
        self.global.unix_time() as f64
    }

    fn is_test_dc(&self) -> bool {
        self.global.is_test_dc()
    }

    fn get_restriction_options(&self) -> crate::restriction_reason::RestrictionOptions {
        crate::restriction_reason::RestrictionOptions {
            ignored_restriction_reasons: String::new(),
            restriction_add_platforms: String::new(),
            ignore_platform_restrictions: false,
        }
    }

    fn get_message_sender_object_const(
        &self,
        sender_user_id: UserId,
        sender_dialog_id: DialogId,
        _source: &str,
    ) -> Option<td_api::MessageSender> {
        if sender_dialog_id.is_valid() {
            return Some(td_api::MessageSender::messageSenderChat(
                td_api::messageSenderChat {
                    chat_id: sender_dialog_id.get(),
                },
            ));
        }
        if !sender_user_id.is_valid() {
            return None;
        }
        Some(td_api::MessageSender::messageSenderUser(
            td_api::messageSenderUser {
                user_id: sender_user_id.get(),
            },
        ))
    }

    fn have_min_user(&self, user_id: UserId) -> bool {
        self.users.contains_key(&user_id.get())
    }

    fn have_dialog(&self, dialog_id: DialogId) -> bool {
        self.messages_manager.get_dialog(dialog_id).is_some()
    }

    fn have_dialog_info(&self, _dialog_id: DialogId) -> bool {
        false
    }

    fn have_min_channel(&self, _channel_id: ChannelId) -> bool {
        false
    }

    fn force_create_dialog(&self, _dialog_id: DialogId, _source: &str) {
        // the C++ force-creates with a `send_closure` — the actor wiring is
        // the Td receive slice's; the intake registers the dialog itself.
    }

    fn content_context_mut(&mut self) -> &mut dyn crate::message_content::MessageContentContext {
        self
    }

    fn have_channel(&self, _channel_id: ChannelId) -> bool {
        false
    }

    fn get_min_channel(
        &self,
        _channel_id: ChannelId,
    ) -> Option<crate::messages_manager::MinChannel> {
        None
    }

    fn can_dialog_have_threads(&self, _dialog_id: DialogId) -> bool {
        false
    }

    fn get_message_id_of_ephemeral_message_id(
        &self,
        dialog_id: DialogId,
        ephemeral_message_id: crate::messages_manager::EphemeralMessageId,
    ) -> crate::messages_manager::MessageId {
        self.messages_manager
            .update_ephemeral_message_ids
            .get(&crate::messages_manager::EphemeralMessageFullId::new(
                dialog_id,
                ephemeral_message_id,
            ))
            .copied()
            .unwrap_or_default()
    }

    fn get_channel_has_linked_channel(&self, _channel_id: ChannelId) -> bool {
        false
    }

    fn is_forum_channel(&self, _channel_id: ChannelId) -> bool {
        false
    }

    fn is_megagroup_channel(&self, _channel_id: ChannelId) -> bool {
        false
    }

    fn get_channel_linked_channel_id(&self, _channel_id: ChannelId, _source: &str) -> ChannelId {
        ChannelId::default()
    }

    fn have_channel_force(&self, _channel_id: ChannelId, _source: &str) -> bool {
        false
    }

    fn have_input_peer_channel(&self, _channel_id: ChannelId) -> bool {
        false
    }

    fn get_user_private_forward_name(&self, _user_id: UserId) -> String {
        String::new()
    }

    fn is_admined_monoforum_channel(&self, _dialog_id: DialogId) -> bool {
        false
    }

    fn is_monoforum_channel_dialog(&self, _dialog_id: DialogId) -> bool {
        false
    }

    fn get_monoforum_channel_id(&self, channel_id: ChannelId) -> ChannelId {
        channel_id
    }

    fn channel_can_post_messages(&self, _channel_id: ChannelId) -> bool {
        false
    }

    fn get_dialog_has_protected_content_force(&self, _dialog_id: DialogId) -> bool {
        false
    }

    fn get_forum_topic_id_object(
        &self,
        _dialog_id: DialogId,
        forum_topic_id: crate::messages_manager::ForumTopicId,
    ) -> i32 {
        forum_topic_id.get()
    }

    fn get_saved_messages_topic_id_object(
        &self,
        _dialog_id: DialogId,
        saved_messages_topic_id: crate::messages_manager::SavedMessagesTopicId,
    ) -> i64 {
        saved_messages_topic_id.get_dialog_id().get()
    }

    fn get_message_content_media_duration(
        &self,
        _content: &crate::message_content::MessageContent,
    ) -> i32 {
        -1
    }

    fn get_message_sender_object_force(
        &self,
        dialog_id: DialogId,
        _source: &str,
    ) -> Option<td_api::MessageSender> {
        Some(td_api::MessageSender::messageSenderChat(
            td_api::messageSenderChat {
                chat_id: dialog_id.get(),
            },
        ))
    }

    fn get_scope_disable_pinned_message_notifications(&self) -> bool {
        false
    }

    fn get_scope_disable_mention_notifications(&self) -> bool {
        false
    }
}

/// The split-context aliasing adapter — the M.10 split functions take
/// their ctx and state objects as separate trait parameters over the same
/// `Td` (the C++ one-object model). The alias below is derived from the
/// single `&mut Td` the arm holds and the pair is used strictly
/// sequentially: every M.10 split function invokes one surface at a time.
/// The full sink-state refactor (a field-destructure content-context
/// view) retires this adapter.
///
/// # Safety
/// The returned alias and the source reference must never be written
/// through reentrantly — the M.10 split functions satisfy this by
/// construction (they pass each object into one callee at a time).
pub(crate) unsafe fn split_context_alias<'a>(
    td: *mut Td,
) -> &'a mut dyn crate::messages_manager::ReadStateContext {
    unsafe { &mut *td }
}

/// The Td-side [`crate::messages_manager::ReadStateContext`] — the
/// M.10.20 state half of the split-context convention. Real routes: the
/// update channel (`Td::send_update`), the option reads, the pmc, the
/// input-peer gate and the message-database gate. The manager-touching
/// hooks (`on_unread_count_change`-family, `update_dialog_lists`) and the
/// M.13 notification hooks answer the documented stand-ins until their
/// receive slices flip (the same shape the test contexts pin).
impl crate::messages_manager::ReadStateContext for Td {
    fn send_update(&mut self, update: td_api::Object) {
        Td::send_update(self, update);
    }

    fn have_input_peer(&self, dialog_id: DialogId, allow_secret_chats: bool) -> bool {
        if dialog_id.get_type() == crate::messages_manager::DialogType::SecretChat
            && !allow_secret_chats
        {
            return false;
        }
        crate::messages_manager::have_dialog_force(&self.messages_manager, dialog_id)
    }

    fn is_dialog_muted(&self, _dialog_id: DialogId) -> bool {
        // NotificationSettingsManager::get_dialog_mute_until — the M.13
        // receive flip
        false
    }

    fn is_dialog_sponsored(&self, _dialog_id: DialogId) -> bool {
        false
    }

    fn on_unread_count_change(
        &mut self,
        _dialog_id: DialogId,
        _old_unread_count: i32,
        _new_unread_count: i32,
        _is_marked_as_unread: bool,
        _force_update: bool,
        _source: &str,
    ) {
    }

    fn on_is_marked_as_unread_change(&mut self, _dialog_id: DialogId, _delta: i32, _source: &str) {}

    fn have_dialog_filters(&self) -> bool {
        false
    }

    fn update_dialog_lists(&mut self, _dialog_id: DialogId, _source: &str) {}

    fn option_integer(&self, name: &str, default: i64) -> i64 {
        self.option_manager
            .as_ref()
            .map(|options| options.get_option_integer(name, default))
            .unwrap_or(default)
    }

    fn option_boolean(&self, name: &str, default: bool) -> bool {
        self.option_manager
            .as_ref()
            .map(|options| options.get_option_boolean(name, default))
            .unwrap_or(default)
    }

    fn binlog_pmc_get(&mut self, key: &str) -> Option<String> {
        let value = self.global.binlog_pmc().get(key);
        if value.is_empty() { None } else { Some(value) }
    }

    fn binlog_pmc_set(&mut self, key: &str, value: &str) {
        self.global.binlog_pmc().set(key, value);
    }

    fn use_message_database(&self) -> bool {
        self.global.use_message_database()
    }

    fn get_dialog_filter_folder_ids(
        &self,
        _filter_id: crate::messages_manager::DialogFilterId,
    ) -> Vec<crate::messages_manager::FolderId> {
        Vec::new()
    }

    fn need_dialog_in_filter(
        &self,
        _filter_id: crate::messages_manager::DialogFilterId,
        _info: &crate::messages_manager::DialogFilterDialogInfo,
    ) -> bool {
        true
    }

    fn is_dialog_pinned_in_filter(
        &self,
        _filter_id: crate::messages_manager::DialogFilterId,
        _dialog_id: DialogId,
    ) -> bool {
        false
    }

    fn get_pinned_dialog_ids_in_filter(
        &self,
        _filter_id: crate::messages_manager::DialogFilterId,
    ) -> Vec<DialogId> {
        Vec::new()
    }

    fn get_scope_disable_mention_notifications(&self) -> bool {
        false
    }

    fn is_bot(&self) -> bool {
        crate::message_content::MessageContentContext::is_bot(self)
    }

    fn close_flag(&self) -> bool {
        self.close_flag != 0
    }

    fn get_sponsored_chat_source_object(&self) -> Option<td_api::ChatSource> {
        None
    }

    fn is_running_get_difference(&self) -> bool {
        false
    }

    fn is_running_get_channel_difference(&self, _dialog_id: DialogId) -> bool {
        false
    }

    fn has_channel_difference_to_log_event(&self, _dialog_id: DialogId) -> bool {
        false
    }

    fn remove_notification_group(
        &mut self,
        _group_id: crate::messages_manager::NotificationGroupId,
        _max_notification_message_id: crate::messages_manager::MessageId,
        _total_count: i32,
        _force: bool,
        _source: &str,
    ) {
        // the M.13 notification flip
    }

    fn remove_notification(
        &mut self,
        _group_id: crate::messages_manager::NotificationGroupId,
        _notification_id: crate::messages_manager::NotificationId,
        _is_permanent: bool,
        _force_update: bool,
    ) {
    }

    fn flush_pending_new_message_notifications(
        &mut self,
        _dialog_id: DialogId,
        _from_mentions: bool,
        _settings_dialog_id: DialogId,
    ) {
    }

    fn remove_dialog_pinned_message_notification(&mut self, _dialog_id: DialogId, _source: &str) {}
}

/// The reactions half of the receive adapter: the banned-chat read is the
/// ChatManager status cache, false until that receive slice lands.
/// The Td-side [`crate::messages_manager::SendMessageContext`] adapter —
/// the M.10.20 dispatch flip. The real reads are the Td-owned surfaces
/// (clock, auth, the messages_manager registry); the manager-facade reads
/// (titles, signatures, slow mode, boosts) answer the documented
/// stand-ins until their const surfaces land with the sink-state
/// refactor; the Wave-2/db reads (stories, forwarded info) answer the
/// same stand-ins as the test contexts.
impl crate::messages_manager::SendMessageContext for Td {
    fn unix_time(&self) -> i32 {
        self.global.unix_time()
    }

    fn is_broadcast_channel_dialog(&self, dialog_id: DialogId) -> bool {
        dialog_id.get_type() == crate::messages_manager::DialogType::Channel
            && crate::message_content::MessageContentContext::is_broadcast_channel(
                self,
                dialog_id.get_channel_id(),
            )
    }

    fn ignore_default_disable_notification(&self) -> bool {
        false
    }

    fn get_channel_sign_messages(&self, _channel_id: ChannelId) -> bool {
        false
    }

    fn get_channel_show_message_sender(&self, _channel_id: ChannelId) -> bool {
        false
    }

    fn is_anonymous_administrator(&self, _dialog_id: DialogId) -> Option<String> {
        None
    }

    fn get_dialog_title(&self, _dialog_id: DialogId) -> String {
        String::new()
    }

    fn get_user_title(&self, user_id: UserId) -> String {
        self.user_manager.get_user_title_of(user_id)
    }

    fn get_secret_chat_ttl(&self, secret_chat_id: SecretChatId) -> i32 {
        self.user_manager.get_secret_chat_ttl(secret_chat_id)
    }

    fn get_channel_my_boost_count(&self, _channel_id: ChannelId) -> i32 {
        0
    }

    fn can_send_message_to_dialog(&self, _dialog_id: DialogId) -> Result<(), (i32, String)> {
        // the real table reads the manager state through a mutable Td —
        // the sink-state refactor lands it; the local-answer flips are
        // unaffected (they never send)
        Ok(())
    }

    fn have_input_peer_read(&self, dialog_id: DialogId, allow_secret_chats: bool) -> bool {
        // the DialogManager facade read needs `&mut Td` (its net/db
        // fallbacks); the in-memory form rides the chat registry
        if dialog_id.get_type() == crate::messages_manager::DialogType::SecretChat
            && !allow_secret_chats
        {
            return false;
        }
        crate::messages_manager::have_dialog_force(&self.messages_manager, dialog_id)
    }

    fn can_send_message_to_forum_topic(
        &self,
        _dialog_id: DialogId,
        _forum_topic_id: crate::messages_manager::ForumTopicId,
    ) -> bool {
        true
    }

    fn can_be_forum(&self, _dialog_id: DialogId) -> bool {
        false
    }

    fn have_dialog_force(&self, dialog_id: DialogId, _source: &str) -> bool {
        crate::messages_manager::have_dialog_force(&self.messages_manager, dialog_id)
    }

    fn get_my_dialog_id(&self) -> DialogId {
        crate::message_content::MessageContentContext::get_my_dialog_id(self)
    }

    fn get_saved_messages_topic_id_from_raw(
        &self,
        topic_id: i64,
    ) -> crate::messages_manager::SavedMessagesTopicId {
        crate::messages_manager::SavedMessagesTopicId::new(DialogId::new(topic_id))
    }

    fn get_forwarded_message_info(
        &self,
        _dialog_id: DialogId,
        _message_id: crate::messages_manager::MessageId,
    ) -> Option<crate::messages_manager::ForwardedMessageInfo> {
        // the message-db load — the db seam
        None
    }

    fn get_message_by_random_id_from_database(
        &self,
        _dialog_id: DialogId,
        _random_id: i64,
    ) -> Option<Box<crate::messages_manager::Message>> {
        None
    }

    fn have_story(&self, _story_full_id: StoryFullId) -> bool {
        false
    }

    fn is_inaccessible_story(&self, _story_full_id: StoryFullId) -> bool {
        false
    }

    fn get_story_duration(&self, _story_full_id: StoryFullId) -> i32 {
        0
    }

    fn get_channel_slow_mode_delay(&self, _channel_id: ChannelId) -> i32 {
        0
    }

    fn is_channel_administrator(&self, _channel_id: ChannelId) -> bool {
        false
    }

    fn on_update_channel_slow_mode_next_send_date(
        &self,
        _channel_id: ChannelId,
        _next_send_date: i32,
    ) {
    }

    fn remove_inactive_channel(&self, _channel_id: ChannelId) {}
}

impl crate::messages_manager::DialogReactionsContext for Td {
    fn is_chat_banned(&self, _dialog_id: DialogId) -> bool {
        false
    }

    fn is_bot(&self) -> bool {
        crate::message_content::MessageContentContext::is_bot(self)
    }
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
    /// The C++ `Td::alarm_manager_` (`AlarmManager`, created in
    /// `Td::start_up`, Td.cpp:454): alarm id → (deadline, the parked ok
    /// promise). The per-alarm `MultiTimeout` shares the Td's single actor
    /// timeout slot here, like the other deadline sources; `setAlarm` is a
    /// preinitialization request, so the map works before `init` — the C++
    /// actor exists from `start_up` for the same reason.
    pending_alarms: BTreeMap<i64, (f64, Promise<()>)>,
    /// `AlarmManager::alarm_id_` — the monotonically increasing alarm key.
    next_alarm_id: i64,
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
    /// The four M.3.9.2 option-read receiver states (trial parameters,
    /// freeze state, age verification, group-call message limits) — the
    /// stand-ins for the TranscriptionManager / UserManager /
    /// AccountManager / GroupCallManager fields until those managers
    /// land (`crate::manager_option_states`).
    pub(crate) manager_states: ManagerOptionStates,
    /// The C++ `Td::link_manager_` (`make_unique` at `Td.cpp:1239`, the
    /// `init_managers` block) — the M.3.9.1 autologin-token slice; the
    /// link surface is Wave 2.
    pub(crate) link_manager: Option<LinkManager>,
    pub(crate) auth_manager: AuthManager,
    /// `Td::user_manager_` — the user/secret-chat cache (created with Td,
    /// loaded from the binlog pmc in `init` like the C++ constructor).
    pub(crate) user_manager: UserManager,
    /// `Td::notification_manager_` (`Td.cpp:1251`).
    pub(crate) notification_manager: NotificationManager,
    /// `Td::notification_settings_manager_` (`Td.cpp:1254`, M.13S).
    pub(crate) notification_settings_manager:
        crate::notification_settings_manager::NotificationSettingsManager,
    /// `Td::device_token_manager_` (`Td.cpp:1345`, M.13S).
    pub(crate) device_token_manager: crate::device_token_manager::DeviceTokenManager,
    /// The notification manager's `send_update` outbox — the collapsed
    /// `send_closure(G()->td(), &Td::send_update, ...)` hop; drained by
    /// [`Td::flush_user_updates`].
    pub(crate) notification_updates: Vec<td_api::Update>,
    /// The premium-required-to-contact QueryMerger actor (a UserManager
    /// member in the C++; the Rust merger is an actor created by `init` —
    /// the `ActorOwn` keeps it alive, dropping it would hang the merged
    /// queries).
    #[allow(dead_code)]
    premium_required_contact_merger: Option<ActorOwn<QueryMerger>>,
    /// `Td::chat_manager_` — the basic-group/supergroup cache (created with
    /// Td; its binlog replay rides the same `process_binlog_events` pass as
    /// the users, in the C++ users→chats→channels order).
    pub(crate) chat_manager: crate::chat_manager::ChatManager,
    /// `Td::dialog_manager_` — the M.6 facade state (recent-dialog lists +
    /// search hints); the actor-dependent halves land with M.6.18+.
    pub(crate) dialog_manager: DialogManager,
    /// `Td::updates_manager_` (`create_actor` at `Td.cpp:1319-1321`, after
    /// the config) — the update engine. The binlog-pmc state loads lazily
    /// on the first pts access (`init_state`), and the pmc writes flush
    /// through `timeout_expired` exactly like the C++ actor's
    /// `MAX_PTS_SAVE_DELAY` throttle.
    pub(crate) updates_manager: UpdatesManager<TdSink>,
    /// `Td::messages_manager_` — the M.10 manager state (the dialog/message
    /// container set + the database family bookkeeping); the receive path
    /// (M.10.3+) is the first reader.
    pub(crate) messages_manager: crate::messages_manager::MessagesManager,
    /// `Td::stop_cnt_` — the hangup and the close completion each drop one.
    stop_cnt: i32,
    /// The deferred-response outbox. The C++ request promises re-enter the
    /// Td actor through `send_closure(td_id_, &Td::send_result/send_error)`;
    /// the collapsed manager calls run inside a `&mut Td` borrow, so their
    /// promise completions push here and the actor drains at the end of the
    /// turn (the `flush_deferred_results` send_closure hop).
    deferred_results: DeferredResults,
    /// The S76 network bridge (spawned by `init`; updates flow back through
    /// `OnNetUpdate`). Idle until the first query, so offline tests stay
    /// offline.
    bridge: Option<NetBridge>,
    /// The authorized self id (from `auth.authorization`).
    my_id: i64,
    /// The self access hash — `inputPeerUser` for the Saved Messages peer
    /// (bots reject `inputPeerSelf` with USER_IS_BOT).
    my_access_hash: i64,
    /// The known users (the self user; contacts land with Wave 1).
    users: HashMap<i64, td_api::User>,
    /// The known chats (Saved Messages only).
    chats: HashMap<i64, td_api::Chat>,
    /// user id → access hash (the peers the walking skeleton messages).
    peer_access: HashMap<i64, i64>,
    /// `get_next_yet_unsent_message_id` — negative, decreasing.
    next_local_message_id: i64,
    /// random_id → the pending send.
    pending_sends: HashMap<i64, PendingSend>,
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

/// The [`UpdatesManagerEnv`] over the `Td` fields: built per drive with
/// the field-split borrows (the `UserEnv` convention), never held across
/// calls.
pub(crate) struct TdUpdatesEnv<'a> {
    pub(crate) global: &'a mut Global,
    pub(crate) options: &'a mut OptionManager,
    auth_is_authorized: bool,
    auth_is_bot: bool,
    my_id: i64,
    can_ignore_background_updates: bool,
}

impl UpdatesManagerEnv for TdUpdatesEnv<'_> {
    fn now(&mut self) -> f64 {
        tdutils::time::now()
    }

    fn unix_time(&mut self) -> i32 {
        self.global.unix_time()
    }

    fn close_flag(&mut self) -> bool {
        self.global.close_flag()
    }

    fn is_authorized(&mut self) -> bool {
        self.auth_is_authorized
    }

    fn is_bot(&mut self) -> bool {
        self.auth_is_bot
    }

    /// `Td::ignore_background_updates()` — the option-gated td flag lands
    /// with the background-updates step; the C++ default is false.
    fn ignore_background_updates(&mut self) -> bool {
        false
    }

    fn can_ignore_background_updates(&mut self) -> bool {
        self.can_ignore_background_updates
    }

    fn get_option_integer(&mut self, name: &str) -> i64 {
        self.options.get_option_integer(name, 0)
    }

    fn set_option_empty(&mut self, name: &str) {
        self.options.set_option_empty(name);
    }

    fn pmc_get(&mut self, key: &str) -> String {
        self.global.binlog_pmc().get(key)
    }

    fn pmc_set(&mut self, key: &str, value: &str) {
        self.global.binlog_pmc().set(key, value);
    }

    fn pmc_erase(&mut self, key: &str) {
        self.global.binlog_pmc().erase(key);
    }

    fn get_my_id(&mut self) -> i64 {
        self.my_id
    }

    /// The `GetDifferenceQuery` — `[blocked: net]` until the S76
    /// bridge↔dispatcher reconciliation; the recorded intent replaces the
    /// query send.
    fn send_get_difference(&mut self, pts: i32, date: i32, qts: i32) {
        tdutils::log_info!(
            "Send GetDifference(pts = {pts}, date = {date}, qts = {qts}) — [blocked: net]"
        );
    }

    /// The `GetUpdatesStateQuery` — `[blocked: net]`.
    fn send_get_updates_state(&mut self) {
        tdutils::log_info!("Send GetUpdatesState — [blocked: net]");
    }

    /// The `ConfirmPtsQtsQuery` — `[blocked: net]`.
    fn send_confirm_pts_qts(&mut self, pts: i32, qts: i32) {
        tdutils::log_info!("Send ConfirmPtsQts(pts = {pts}, qts = {qts}) — [blocked: net]");
    }

    /// The `GetPtsUpdateQuery` — `[blocked: net]`.
    fn send_get_pts_update(&mut self, pts: i32) {
        tdutils::log_info!("Send GetPtsUpdate(pts = {pts}) — [blocked: net]");
    }

    /// The `PingServerQuery` — `[blocked: net]`.
    fn send_ping_server(&mut self) {
        tdutils::log_info!("Send PingServer — [blocked: net]");
    }

    /// `td_->online_manager_->is_online()` — the online manager lands
    /// with the net step; offline is the safe default for the reloader
    /// gate.
    fn is_online(&mut self) -> bool {
        false
    }
}

/// The deferred-response outbox shared with the request promises (see the
/// field docs).
type DeferredResults = Arc<Mutex<Vec<(u64, Result<Object, Status>)>>>;

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
            pending_alarms: BTreeMap::new(),
            next_alarm_id: 0,
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
            manager_states: ManagerOptionStates::default(),
            link_manager: None,
            auth_manager: AuthManager::new(),
            user_manager: UserManager::new(),
            notification_manager: NotificationManager::default(),
            notification_settings_manager:
                crate::notification_settings_manager::NotificationSettingsManager::new(),
            device_token_manager: crate::device_token_manager::DeviceTokenManager::new(),
            notification_updates: Vec::new(),
            premium_required_contact_merger: None,
            chat_manager: crate::chat_manager::ChatManager::new(),
            dialog_manager: DialogManager::new(),
            updates_manager: UpdatesManager::new(TdSink::default()),
            messages_manager: crate::messages_manager::MessagesManager::new(),
            stop_cnt: 2,
            deferred_results: Arc::new(Mutex::new(Vec::new())),
            bridge: None,
            my_id: 0,
            my_access_hash: 0,
            users: HashMap::new(),
            chats: HashMap::new(),
            peer_access: HashMap::new(),
            next_local_message_id: -1,
            pending_sends: HashMap::new(),
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
        // A manager call inside the request may have armed or cancelled a
        // user deadline (UserManager.cpp's member MultiTimeouts re-arm on
        // every set/cancel) — point the actor timeout at the earliest one.
        self.rearm_user_timeouts();
        // The manager mutations broadcast `updateUser`-class updates
        // immediately in the C++ (send_closure from the manager); the
        // collapse queues them. The deferred promise completions answer
        // through the same end-of-turn hop.
        self.flush_user_updates();
        self.flush_deferred_results();
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
                    // (the variant match avoids the generated Object →
                    // Update downcast: it moves the 32 KB enum through a
                    // frame-sized match and exhausts the stack in debug)
                    let mut updates: Vec<td_api::Update> = Vec::new();
                    for update in self.get_fake_current_state() {
                        match update {
                            Object::updateOption(u) => {
                                updates.push(td_api::Update::updateOption(u))
                            }
                            Object::updateAuthorizationState(u) => {
                                updates.push(td_api::Update::updateAuthorizationState(u))
                            }
                            _ => unreachable!("the fake state is option/auth updates only"),
                        }
                    }
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

    /// `Td::init` (Td.cpp:835-962), the M.1 subset in the C++ order with the
    /// S76 bridge/resume wiring.
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
        let user_events = std::mem::take(&mut opened_database.user_events);
        let secret_chat_events = std::mem::take(&mut opened_database.secret_chat_events);
        let notification_settings_events =
            std::mem::take(&mut opened_database.notification_settings_events);
        let chat_events = std::mem::take(&mut opened_database.chat_events);
        let channel_events = std::mem::take(&mut opened_database.channel_events);
        let dialog_manager_events = std::mem::take(&mut opened_database.dialog_manager_events);
        let messages_manager_events = std::mem::take(&mut opened_database.messages_manager_events);

        if self.state == State::Close {
            // Close asynchronously opened database
            opened_database.database.close_all(self.destroy_flag);
            return self.finish_set_parameters();
        }

        self.global.init(opened_database.database);

        // `DeviceTokenManager::start_up` (DeviceTokenManager.cpp:329) —
        // the pmc token load runs at Td start, auth-independently; the
        // sync loop is the [blocked: net] seam.
        self.load_device_token_states();

        // `Td::process_binlog_events` (Td.cpp:1034) — the on-server
        // notification settings log events replay into the settings
        // manager; the sends are the [blocked: net] seams.
        {
            let mut env = NotificationEnv {
                global: &mut self.global,
                options: self.option_manager.as_mut(),
                auth_is_authorized: self.auth_manager.is_authorized(),
                auth_is_bot: self.auth_manager.is_bot(),
                updates: &mut self.notification_updates,
            };
            self.notification_settings_manager
                .on_binlog_events(&mut env, notification_settings_events);
        }

        // The S76 bridge thread (updates flow back through OnNetUpdate).
        // Idle until the first query, so the offline tests stay offline.
        // Spawned only under an actor scheduler (the live runs); the
        // direct-call tests drive `Td::init` without one, and both
        // `actor_id_of` and `instance_ptr` need the scheduler TLS.
        if Scheduler::in_scheduler_context() {
            let actor_id = self.actor_id_of::<Td>();
            let config = BridgeConfig {
                api_id: parameters.api_id,
                api_hash: parameters.api_hash.clone(),
                use_test_dc: parameters.use_test_dc,
                database_directory: parameters.database_directory.clone(),
                device_model: self.header_options.device_model.clone(),
                system_version: self.header_options.system_version.clone(),
                application_version: self.header_options.application_version.clone(),
                system_language_code: self.header_options.system_language_code.clone(),
            };
            self.bridge = Some(NetBridge::spawn(
                config,
                actor_id,
                Scheduler::instance_ptr(),
            ));
        }

        // A persisted walking-skeleton session resumes straight to Ready
        // (the C++ AuthManager ctor reading the binlog "auth" key — zero
        // network); the self user is re-queried over the bridge.
        let resumed_user = if NetBridge::has_persisted_state(&parameters.database_directory) {
            self.query_users_me()
        } else {
            None
        };

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
            if let Some(user) = &resumed_user {
                // `UserManager::set_my_id` — the C++ config pmc persists it
                // and the option storm on reopen carries it.
                option_manager.set_option_integer("my_id", user.id);
            }
            self.option_manager = Some(option_manager);
        }
        // The option storm starts with unix_time (the C++ ctor broadcasts
        // it before utc_time_offset and the store entries).
        self.send_update(Object::updateOption(td_api::updateOption {
            name: "unix_time".to_string(),
            value: Some(td_api::OptionValue::optionValueInteger(
                td_api::optionValueInteger {
                    value: tdutils::time::now() as i64,
                },
            )),
        }));
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
        if let Some(user) = resumed_user {
            // The walking-skeleton resume: Ready + the cached self user (the
            // C++ emits Ready from the binlog restore and reads the user
            // from the UserManager cache — no updateUser broadcast).
            self.auth_manager.set_state_ok();
            self.apply_self_user(&user, false);
            self.init_notification_manager();
        } else {
            let binlog_pmc = self.global.binlog_pmc();
            let auth_str = binlog_pmc.get("auth");
            match auth_str.as_str() {
                "ok" => {
                    let is_bot = binlog_pmc.get("auth_is_bot") == "true";
                    // `UserManager::load_my_id()` + the my_id option
                    // (AuthManager.cpp:342-348); the fallback
                    // `send_get_me_query` needs the net step.
                    let my_id = UserManager::load_my_id(binlog_pmc);
                    if my_id.is_valid() {
                        tracing::info!("Logged in as {my_id:?}");
                        if let Some(option_manager) = self.option_manager.as_mut() {
                            option_manager.set_option_integer("my_id", my_id.get());
                        }
                    } else {
                        tdutils::log_error!("Restore unknown my_id");
                    }
                    self.auth_manager
                        .restore_from_pmc("ok", is_bot, String::new());
                    self.init_notification_manager();
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
        // each manager step appends its block here in the C++ order. The
        // UserManager constructor loads my_id from the binlog pmc
        // (UserManager.cpp:2824 `my_id_ = load_my_id()`) and the
        // contacts sync state (cpp:2748-2759); the freeze-state pmc read
        // lands with the M.4.15 step.
        self.user_manager.init_my_id(self.global.binlog_pmc());
        let unix_time = self.global.unix_time();
        let use_chat_info_database = self.global.use_chat_info_database();
        let auth_is_bot = self.auth_manager.is_bot();
        self.user_manager.init_contacts_state(
            self.global.binlog_pmc(),
            unix_time,
            use_chat_info_database,
            auth_is_bot,
        );
        if !self.auth_manager.is_bot() {
            // UserManager.cpp:2765-2771 — the my_was_online pair restore;
            // `is_online` is OnlineManager::is_online() until its step.
            let unix_time = self.global.unix_time();
            self.user_manager
                .init_my_online_status(self.global.binlog_pmc(), unix_time, false);
            // UserManager.cpp:2773-2781 — the freeze-state restore (the
            // updateFreezeState emission rides the flush below).
            self.user_manager
                .init_freeze_state(self.global.binlog_pmc());
        }
        // UserManager.cpp:2829-2845 — start_up's saved-music block; the
        // C++ gate is is_authorized() && !is_bot().
        self.user_manager.init_my_saved_music_ids(
            self.global.binlog_pmc(),
            self.auth_manager.is_authorized() && !self.auth_manager.is_bot(),
        );

        // The M.3.9.2 stand-in for the four manager constructors' pmc
        // reads (in the C++ creation order: AccountManager Td.cpp:1157,
        // GroupCallManager :1231, TranscriptionManager :1313, UserManager
        // :1322) — the states replay from the binlog pmc and send the
        // same updates the C++ constructors do. Each read moves into its
        // manager when that manager lands.
        {
            let is_authorized = self.auth_manager.is_authorized();
            let is_bot = self.auth_manager.is_bot();
            let unix_time = self.global.unix_time();
            let updates = {
                let pmc = self.global.binlog_pmc();
                self.manager_states
                    .load(pmc, is_authorized, is_bot, unix_time)
            };
            for update in updates {
                self.send_update(update);
            }
        }

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

        // The UserManager member actors (the C++ `get_is_premium_required_
        // to_contact_queries_` value member + its `set_merge_function`;
        // the Rust merger is an actor whose merge function messages the Td
        // actor to build the input users and send the query). Both need the
        // scheduler context — the direct-call tests run bare, leaving the
        // merger unwired (its uncached consumers answer the "Request
        // aborted" surface, like the C++ close state).
        if tdactor::Scheduler::in_scheduler_context() {
            let td_id = self.actor_id_of::<Td>();
            let merger = tdactor::create_actor(
                "GetIsPremiumRequiredToContactMerger",
                QueryMerger::new(
                    3,
                    100,
                    Box::new(move |query_ids, promise| {
                        tdactor::send_closure(
                            td_id.clone(),
                            MergePremiumRequiredContactQueries {
                                user_ids: query_ids,
                                promise,
                            },
                        );
                    }),
                ),
            );
            self.user_manager.get_is_premium_required_to_contact_queries = Some(merger.get());
            self.premium_required_contact_merger = Some(merger);
            self.user_manager.td_actor_id = Some(self.actor_id_of::<Td>());
        }

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

        // process_binlog_events (Td.cpp:966-990): the users vector replays
        // first, then the secret-chat infos vector (the other handlers
        // arrive with their managers).
        {
            let global = &mut self.global;
            let options = self
                .option_manager
                .as_mut()
                .expect("option manager exists after init");
            let mut env = UserEnv {
                global,
                options,
                auth_is_bot: self.auth_manager.is_bot(),
            };
            for event in &user_events {
                self.user_manager.on_binlog_user_event(&mut env, event);
            }
            for event in &secret_chat_events {
                self.user_manager
                    .on_binlog_secret_chat_event(&mut env, event);
            }
            // the C++ replays the chats vector, then the channels vector
            // (Td.cpp:966-990, users→chats→channels order); the chat
            // handlers ride the M.5.18 diff machine, the channel handlers
            // run the save fan-out directly until M.5.19 lands
            for event in &chat_events {
                self.chat_manager
                    .on_binlog_chat_event(&mut self.user_manager, &mut env, event);
            }
            for event in &channel_events {
                self.chat_manager
                    .on_binlog_channel_event(&mut self.user_manager, &mut env, event);
            }
        }
        for event in &dialog_manager_events {
            crate::dialog_manager::on_binlog_dialog_events(self, std::slice::from_ref(event));
        }
        // the `to_messages_manager` vector (Td.cpp:964-990 sends it to the
        // MessagesManager actor; the port replays in place): the ported
        // handler is the DeleteMessage binlog replay arm — the reopened
        // engine re-applies the durable delete (MessagesManager.cpp:36248-36264).
        for event in &messages_manager_events {
            if event.type_ == tddb::td_db::LOG_EVENT_HANDLER_TYPE_DELETE_MESSAGE {
                let mut manager = std::mem::take(&mut self.messages_manager);
                crate::messages_manager::replay_delete_message_log_event(
                    &mut self.global,
                    &mut manager,
                    event,
                );
                self.messages_manager = manager;
            }
        }
        self.flush_user_updates();

        // The init ping: country code pre-auth — needs the net step. The
        // authorized branch is real (Td.cpp:951): the engine replays the
        // pmc state and, with the senders `[blocked: net]`, records the
        // GetDifference intent instead of sending it.
        if !self.auth_manager.is_authorized() {
            // country_info_manager_->get_current_country_code — Wave 2.
        } else {
            let global = &mut self.global;
            let options = self
                .option_manager
                .as_mut()
                .expect("option manager exists after init");
            let mut env = TdUpdatesEnv {
                global,
                options,
                auth_is_authorized: self.auth_manager.is_authorized(),
                auth_is_bot: self.auth_manager.is_bot(),
                my_id: self.user_manager.get_my_id().get(),
                can_ignore_background_updates: self.can_ignore_background_updates,
            };
            self.updates_manager.get_difference(&mut env, "init");
        }

        tracing::info!("Finish initialization");

        self.state = State::Run;

        let request_id = self.set_parameters_request_id;
        self.send_result(request_id, ok_object());
        self.finish_set_parameters();

        // The queued preauth requests re-run through the live dispatch (the
        // C++ recorder pins: ok, then the preauth replay, then the
        // connection state).
        self.complete_pending_preauthentication_requests(&[FunctionKind::Any]);

        // BackgroundManager / FileManager init constants.
        self.send_update(Object::updateDefaultBackground(
            td_api::updateDefaultBackground {
                for_dark_theme: false,
                background: None,
            },
        ));
        self.send_update(Object::updateDefaultBackground(
            td_api::updateDefaultBackground {
                for_dark_theme: true,
                background: None,
            },
        ));
        self.send_update(Object::updateFileDownloads(td_api::updateFileDownloads {
            total_size: 0,
            total_count: 0,
            downloaded_size: 0,
        }));

        // ConnectionStateManager::start_up: the initial real state.
        self.send_update(Object::updateConnectionState(
            td_api::updateConnectionState {
                state: Some(td_api::ConnectionState::connectionStateConnecting(
                    td_api::connectionStateConnecting,
                )),
            },
        ));
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
                    authorization_state: Some(
                        td_api::AuthorizationState::authorizationStateClosing(
                            td_api::authorizationStateClosing,
                        ),
                    ),
                },
            ));
        } else {
            self.state = State::Close;
            self.close_flag = 1;
            self.global.set_close_flag();
            self.auth_manager.on_closing(self.destroy_flag);
            self.flush_auth_updates();
            self.flush_user_updates();
            // updates_manager_->timeout_expired() — save PTS and QTS
            // before the teardown (Td.cpp:757 resets the actor; the pmc
            // flush is the observable part).
            {
                let global = &mut self.global;
                let options = self
                    .option_manager
                    .as_mut()
                    .expect("option manager exists after init");
                let mut env = TdUpdatesEnv {
                    global,
                    options,
                    auth_is_authorized: self.auth_manager.is_authorized(),
                    auth_is_bot: self.auth_manager.is_bot(),
                    my_id: self.user_manager.get_my_id().get(),
                    can_ignore_background_updates: self.can_ignore_background_updates,
                };
                self.updates_manager.timeout_expired(&mut env);
            }
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

        // `Td::clear` (Td.cpp:653-658): flush or destroy the active
        // notifications before the managers reset
        if !self.auth_manager.is_bot() {
            let mut env = NotificationEnv {
                global: &mut self.global,
                options: self.option_manager.as_mut(),
                auth_is_authorized: self.auth_manager.is_authorized(),
                auth_is_bot: self.auth_manager.is_bot(),
                updates: &mut self.notification_updates,
            };
            if self.destroy_flag {
                self.notification_manager
                    .destroy_all_notifications(&mut env);
            } else {
                self.notification_manager.flush_all_notifications(&mut env);
            }
            self.flush_user_updates();
        }

        // G()->net_query_creator().stop_check() / result_handlers_ /
        // StateManager — with the net step.
        self.clear_requests();

        // `AlarmManager::tear_down` (AlarmManager.cpp:17-28, via the
        // Td.cpp:788 actor reset): every pending alarm's promise errors
        // with the aborted error. `clear_requests` already untracked the
        // ids, so the settled errors re-enter for untracked ids and drop —
        // the C++ promise-actor hangups after `clear` behave the same.
        for (_, (_, mut promise)) in std::mem::take(&mut self.pending_alarms) {
            promise.set_error(tdutils::status::Status::error(
                crate::request_actor::REQUEST_ABORTED_CODE,
                crate::request_actor::REQUEST_ABORTED_MESSAGE,
            ));
        }

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
                authorization_state: Some(td_api::AuthorizationState::authorizationStateClosed(
                    td_api::authorizationStateClosed,
                )),
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
                authorization_state: Some(authorization_state),
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

        // The M.3.9.2 manager fan-outs — the C++ `send_closure`s from
        // `process_app_config`, applied to the Td's manager states after
        // the option broadcasts (the C++ actors deliver them around the
        // option writes; this fixed order is the port's determinism).
        if !actions.manager_updates.is_empty() {
            let is_authorized = self.auth_manager.is_authorized();
            let is_bot = self.auth_manager.is_bot();
            let unix_time = self.global.unix_time();
            for update in actions.manager_updates {
                let sent = {
                    let pmc = self.global.binlog_pmc();
                    self.manager_states.on_manager_update(
                        pmc,
                        update,
                        is_authorized,
                        is_bot,
                        unix_time,
                    )
                };
                if let Some(update) = sent {
                    self.send_update(update);
                }
            }
        }

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
        // The UserManager's deadlines share the same slot (M.4.x): the
        // earliest of the three sources wins.
        if let Some(user_deadline) = self.user_manager.next_deadline()
            && (expire_time == 0.0 || user_deadline < expire_time)
        {
            expire_time = user_deadline;
        }
        // The ChatManager's channel deadlines join the same slot (M.5.14):
        // the earliest of the four sources wins.
        if let Some(chat_deadline) = self.chat_manager.next_deadline()
            && (expire_time == 0.0 || chat_deadline < expire_time)
        {
            expire_time = chat_deadline;
        }
        // The NotificationManager's two timeout families share the same
        // slot (M.13): the fired timeout runs `tick_timeouts`.
        if let Some(notification_deadline) = self.notification_manager.next_deadline()
            && (expire_time == 0.0 || notification_deadline < expire_time)
        {
            expire_time = notification_deadline;
        }
        // The notification-settings scope-unmute and the device-token
        // register-retry deadlines share it too (M.13S).
        if let Some(settings_deadline) = self.notification_settings_manager.next_deadline()
            && (expire_time == 0.0 || settings_deadline < expire_time)
        {
            expire_time = settings_deadline;
        }
        if let Some(token_deadline) = self.device_token_manager.next_deadline()
            && (expire_time == 0.0 || token_deadline < expire_time)
        {
            expire_time = token_deadline;
        }
        // The AlarmManager timers share the same slot (M.11.4): the
        // earliest armed alarm wins.
        if let Some(alarm_deadline) = self.next_alarm_deadline()
            && (expire_time == 0.0 || alarm_deadline < expire_time)
        {
            expire_time = alarm_deadline;
        }
        // The UpdatesManager's gap/save/retry/reload timers join the same
        // slot (M.7.18).
        if let Some(updates_deadline) = self.updates_manager.next_deadline()
            && (expire_time == 0.0 || updates_deadline < expire_time)
        {
            expire_time = updates_deadline;
        }
        if expire_time == 0.0 {
            self.cancel_timeout();
        } else {
            self.set_timeout_at(expire_time);
        }
    }

    /// `AlarmManager::set_alarm` (AlarmManager.cpp:52-60) behind the
    /// `Requests::on_request(setAlarm)` `CREATE_OK_REQUEST_PROMISE`
    /// (Requests.cpp:8976-8979): the ok answer parks until the deadline.
    pub(crate) fn set_alarm(&mut self, id: u64, seconds: f64) {
        if !(0.0..=3e9).contains(&seconds) {
            return self.send_error_impl(id, 400, "Wrong parameter seconds specified");
        }
        self.next_alarm_id += 1;
        let promise = self.deferred_promise(id, |()| crate::request_actor::ok_object());
        self.pending_alarms.insert(
            self.next_alarm_id,
            (tdutils::time::now() + seconds, promise),
        );
        // the C++ alarm actor arms its own MultiTimeout timer; the shared
        // Td timeout slot needs the mirror (a no-op outside a scheduler)
        self.rearm_user_timeouts();
    }

    /// The earliest armed alarm deadline, for the shared timeout slot.
    fn next_alarm_deadline(&self) -> Option<f64> {
        self.pending_alarms
            .values()
            .map(|(at, _)| *at)
            .min_by(f64::total_cmp)
    }

    /// `AlarmManager::on_alarm_timeout` (AlarmManager.cpp:36-48): every due
    /// alarm answers its parked ok promise.
    fn tick_alarms(&mut self) {
        let now = tdutils::time::now();
        let due: Vec<i64> = self
            .pending_alarms
            .iter()
            .filter(|(_, (at, _))| *at <= now)
            .map(|(alarm_id, _)| *alarm_id)
            .collect();
        for alarm_id in due {
            let (_, (_, mut promise)) = self
                .pending_alarms
                .remove_entry(&alarm_id)
                .expect("due alarm just disappeared");
            promise.set_value(());
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

    /// Drain the UserManager's queued `updateUser`-class broadcasts (the
    /// C++ sends them straight to `Td::send_update` from the manager).
    /// Drain the managers' queued update broadcasts (the C++ sends them
    /// straight to `Td::send_update` from the managers).
    pub(crate) fn flush_user_updates(&mut self) {
        for update in self.user_manager.take_updates() {
            self.send_update(update.into());
        }
        for update in self.chat_manager.take_updates() {
            self.send_update(update.into());
        }
        let notification_updates = std::mem::take(&mut self.notification_updates);
        for update in notification_updates {
            self.send_update(update.into());
        }
    }

    /// `td_->notification_manager_->init()` +
    /// `td_->notification_settings_manager_->init()` (AuthManager.cpp:1545)
    /// — idempotent; the managers no-op while disabled.
    fn init_notification_manager(&mut self) {
        let mut env = NotificationEnv {
            global: &mut self.global,
            options: self.option_manager.as_mut(),
            auth_is_authorized: self.auth_manager.is_authorized(),
            auth_is_bot: self.auth_manager.is_bot(),
            updates: &mut self.notification_updates,
        };
        self.notification_manager.init(&mut env);
        self.notification_settings_manager.init(&mut env);
    }

    /// `DeviceTokenManager::start_up` (DeviceTokenManager.cpp:329) — the
    /// token pmc load runs at Td start, auth-independently; the sync loop
    /// is the [blocked: net] seam.
    fn load_device_token_states(&mut self) {
        let mut env = NotificationEnv {
            global: &mut self.global,
            options: self.option_manager.as_mut(),
            auth_is_authorized: self.auth_manager.is_authorized(),
            auth_is_bot: self.auth_manager.is_bot(),
            updates: &mut self.notification_updates,
        };
        self.device_token_manager.load_states(&mut env);
    }

    /// The deferred-response outbox drain — the `send_closure(td_id_,
    /// &Td::send_result/send_error, id, ...)` hop the C++ request promises
    /// make when a manager completes them (possibly from inside another
    /// manager call, where a `&mut Td` is not available).
    pub(crate) fn flush_deferred_results(&mut self) {
        let deferred = {
            let mut guard = self.deferred_results.lock().unwrap();
            std::mem::take(&mut *guard)
        };
        for (id, result) in deferred {
            match result {
                Ok(object) => self.send_result(id, object),
                Err(status) => self.send_error_impl(id, status.code(), status.message()),
            }
        }
    }

    /// The promise factory for the request layer: a manager-completed
    /// `Result` lands in the outbox under the request id.
    pub(crate) fn deferred_promise<T, F>(&self, id: u64, map: F) -> tdutils::promise::Promise<T>
    where
        T: Send + 'static,
        F: FnOnce(T) -> Object + Send + 'static,
    {
        let outbox = Arc::clone(&self.deferred_results);
        tdutils::promise::PromiseCreator::lambda_result(
            move |result: tdutils::status::Result<T>| {
                let result = match result {
                    tdutils::status::Result::Ok(value) => Ok(map(value)),
                    tdutils::status::Result::Err(status) => Err(status),
                };
                outbox.lock().unwrap().push((id, result));
            },
        )
    }

    /// Point the actor timeout at the earliest armed deadline (the member
    /// MultiTimeouts re-arm on every set/cancel). The slot is shared with
    /// the ConfigManager expire time and the recoverer wakeup, so this
    /// mirrors into `sync_config_timeout` — a no-op outside a scheduler,
    /// like the other direct-call mirroring points.
    fn rearm_user_timeouts(&mut self) {
        self.sync_config_timeout();
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
            use_test_dc,
            database_directory: PathBuf::from(&database_directory),
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

    // --------------------------------------------------------------------
    // The S76 walking-skeleton live paths.
    // --------------------------------------------------------------------

    /// `setAuthenticationPhoneNumber` (Requests.cpp:1786-1794): the clean
    /// check, then `AuthManager::set_phone_number` over the bridge.
    pub(crate) fn set_authentication_phone_number(&mut self, request_id: u64, phone: String) {
        let phone = match crate::misc::clean_input_string(&phone) {
            Some(phone) => phone,
            None => {
                return self.send_error_impl(request_id, 400, "Strings must be encoded in UTF-8");
            }
        };
        let bridge = self.bridge.as_ref().expect("bridge").handle();
        let params = self.parameters.as_ref().expect("parameters");
        let mut sender = AuthSender::new(
            &bridge,
            params.api_id,
            params.api_hash.clone(),
            String::new(),
        );
        match self
            .auth_manager
            .set_authentication_phone_number(&phone, &mut sender)
        {
            Ok(()) => {
                self.flush_auth_updates();
                self.send_result(request_id, ok_object());
            }
            Err(message) => self.send_error_impl(request_id, 400, &message),
        }
    }

    pub(crate) fn check_authentication_bot_token(&mut self, request_id: u64, token: String) {
        let token = match crate::misc::clean_input_string(&token) {
            Some(token) => token,
            None => {
                return self.send_error_impl(request_id, 400, "Strings must be encoded in UTF-8");
            }
        };
        self.check_code_or_password(request_id, |sender, auth| {
            auth.check_authentication_bot_token(&token, sender)
        });
    }

    pub(crate) fn check_authentication_code(&mut self, request_id: u64, code: String) {
        let code = match crate::misc::clean_input_string(&code) {
            Some(code) => code,
            None => {
                return self.send_error_impl(request_id, 400, "Strings must be encoded in UTF-8");
            }
        };
        self.check_code_or_password(request_id, |sender, auth| {
            auth.check_authentication_code(&code, sender)
        });
    }

    pub(crate) fn check_authentication_password(&mut self, request_id: u64, password: String) {
        let password = match crate::misc::clean_input_string(&password) {
            Some(password) => password,
            None => {
                return self.send_error_impl(request_id, 400, "Strings must be encoded in UTF-8");
            }
        };
        self.check_code_or_password(request_id, |sender, auth| {
            auth.check_authentication_password(&password, sender)
        });
    }

    /// `checkAuthenticationCode` / `checkAuthenticationPassword` — the
    /// shared completion: the authorization updates BEFORE the ok, the
    /// success sequence of `AuthManager::on_get_authorization`
    /// (authorization_date, my_id, updateUser(self), Ready).
    fn check_code_or_password(
        &mut self,
        request_id: u64,
        run: impl FnOnce(&mut dyn AuthQuerySender, &mut AuthManager) -> Result<(), String>,
    ) {
        let bridge = self.bridge.as_ref().expect("bridge").handle();
        let params = self.parameters.as_ref().expect("parameters");
        let mut sender = AuthSender::new(
            &bridge,
            params.api_id,
            params.api_hash.clone(),
            String::new(),
        );
        // The password for the SRP step rides the checkAuthenticationPassword
        // request; check_authentication_password carries it through the
        // query (the sender re-fetches account.getPassword for fresh salts).
        let result = {
            let mut auth = std::mem::take(&mut self.auth_manager);
            let outcome = run(&mut sender, &mut auth);
            self.auth_manager = auth;
            outcome
        };
        match result {
            Ok(()) => {
                if self.auth_manager.is_authorized() {
                    // The ConfigManager's `help.getConfig` step: the
                    // config-derived options come from the live reply. A
                    // failed fetch skips them, as in the C++ client when
                    // the config has not arrived yet.
                    let config = sender.get_config().ok();
                    self.complete_authorization(request_id, &sender, config.as_ref());
                } else {
                    // WaitRegistration / WaitPassword — updates then ok.
                    self.flush_auth_updates();
                    self.send_result(request_id, ok_object());
                }
            }
            Err(message) => self.send_error_impl(request_id, 400, &message),
        }
    }

    /// The `AuthManager::on_get_authorization` success broadcast in the
    /// order the live recorder pins: unix_time, the applied test-DC config
    /// options, authorization_date, my_id, the self user, Ready — then the
    /// ok, `connectionStateUpdating`, the default reaction type and
    /// `connectionStateReady`.
    fn complete_authorization(
        &mut self,
        request_id: u64,
        sender: &AuthSender<'_>,
        config: Option<&telegram_api::Config>,
    ) {
        self.send_update(Object::updateOption(td_api::updateOption {
            name: "unix_time".to_string(),
            value: Some(td_api::OptionValue::optionValueInteger(
                td_api::optionValueInteger {
                    value: tdutils::time::now() as i64,
                },
            )),
        }));

        // The ConfigManager-applied options: the `help.getConfig`-derived
        // subset in the C++ `process_config` emission order the recorder
        // pins; the search-bot options are app-config sourced.
        if let Some(option_manager) = self.option_manager.as_mut() {
            if let Some(config) = config {
                option_manager.set_option_boolean("test_mode", config.test_mode);
                option_manager.set_option_integer(
                    "forwarded_message_count_max",
                    config.forwarded_count_max as i64,
                );
                option_manager
                    .set_option_integer("basic_group_size_max", config.chat_size_max as i64);
                option_manager
                    .set_option_integer("supergroup_size_max", config.megagroup_size_max as i64);
                option_manager.set_option_boolean("expect_blocking", config.blocked_mode);
                if !config.me_url_prefix.is_empty() {
                    let mut url = config.me_url_prefix.clone();
                    if !url.ends_with('/') {
                        url.push('/');
                    }
                    option_manager.set_option_string("t_me_url", &url);
                }
                option_manager.set_option_integer(
                    "call_connect_timeout_ms",
                    config.call_connect_timeout_ms as i64,
                );
                option_manager.set_option_integer(
                    "call_packet_timeout_ms",
                    config.call_packet_timeout_ms as i64,
                );
            }
            option_manager.set_option_string("animation_search_bot_username", "contextbot");
            option_manager.set_option_string("venue_search_bot_username", "foursquarebot");
            option_manager.set_option_string("photo_search_bot_username", "imagebot");
        }
        self.flush_option_updates();

        self.send_update(Object::updateOption(td_api::updateOption {
            name: "authorization_date".to_string(),
            value: Some(td_api::OptionValue::optionValueInteger(
                td_api::optionValueInteger {
                    value: tdutils::time::now() as i64,
                },
            )),
        }));
        if let Some(user) = sender.last_user() {
            self.apply_self_user(user, true);
        }
        self.flush_auth_updates();
        self.send_result(request_id, ok_object());

        self.send_update(Object::updateConnectionState(
            td_api::updateConnectionState {
                state: Some(td_api::ConnectionState::connectionStateUpdating(
                    td_api::connectionStateUpdating,
                )),
            },
        ));
        self.send_update(Object::updateDefaultReactionType(
            td_api::updateDefaultReactionType {
                reaction_type: Some(td_api::ReactionType::reactionTypeEmoji(
                    td_api::reactionTypeEmoji {
                        emoji: "❤".to_string(),
                    },
                )),
            },
        ));
        self.send_update(Object::updateConnectionState(
            td_api::updateConnectionState {
                state: Some(td_api::ConnectionState::connectionStateReady(
                    td_api::connectionStateReady,
                )),
            },
        ));

        // The post-Ready position: `updates.getState` seeds the pts (the
        // C++ get_difference chain; the skeleton keeps the position).
        if let Some(owner) = self.bridge.as_ref() {
            let bridge = owner.handle();
            let state = telegram_api::Function::updates_getState(telegram_api::updates_getState);
            if let QueryAnswer::Ok(data) = bridge.query(state)
                && let Ok(m) = telegram_api::updates_State::parse(&data)
            {
                // The M.7 engine: `updates.getState` replays through
                // on_get_updates_state (the C++ get_difference chain);
                // the S72 set_pts/set_qts/set_seq calls are superseded.
                let global = &mut self.global;
                let options = self
                    .option_manager
                    .as_mut()
                    .expect("option manager exists after init");
                let mut env = TdUpdatesEnv {
                    global,
                    options,
                    auth_is_authorized: self.auth_manager.is_authorized(),
                    auth_is_bot: self.auth_manager.is_bot(),
                    my_id: self.user_manager.get_my_id().get(),
                    can_ignore_background_updates: self.can_ignore_background_updates,
                };
                self.updates_manager
                    .on_get_updates_state(&mut env, m, "updates.getState");
            }
            bridge.mark_authorized();
            bridge.persist();
        }

        // the live `on_get_authorization` init (AuthManager.cpp:1545); the
        // C++ fires it after every manager init above — the notification
        // manager is the only one on this tree
        self.init_notification_manager();
    }

    /// Store + broadcast the self user: `UserManager::set_my_id` (the
    /// `my_id` option) then `updateUser` (AuthManager.cpp:1517-1520).
    fn apply_self_user(&mut self, user: &telegram_api::user, broadcast: bool) {
        let object = convert::user_object(user, true);
        self.my_id = object.id;
        self.my_access_hash = user.access_hash;
        if broadcast {
            self.send_update(Object::updateOption(td_api::updateOption {
                name: "my_id".to_string(),
                value: Some(td_api::OptionValue::optionValueInteger(
                    td_api::optionValueInteger { value: self.my_id },
                )),
            }));
        }
        self.users.insert(object.id, object.clone());
        if broadcast {
            self.send_update(Object::updateUser(td_api::updateUser {
                user: Some(object),
            }));
        }
    }

    /// One `users.getUsers[inputUserSelf]` round-trip (the wire `getMe`).
    fn query_users_me(&mut self) -> Option<telegram_api::user> {
        let bridge = self.bridge.as_ref()?.handle();
        let query = telegram_api::Function::users_getUsers(telegram_api::users_getUsers {
            id: vec![telegram_api::InputUser::inputUserSelf(
                telegram_api::inputUserSelf,
            )],
        });
        match bridge.query(query) {
            QueryAnswer::Ok(data) => parse_vector_user(&data).and_then(|mut users| users.pop()),
            _ => None,
        }
    }

    /// `getMe` (Requests.cpp): the cached self user, the UserManager store
    /// (the M.4 surface), or one `users.getUsers` round-trip over the bridge.
    pub(crate) fn get_me(&mut self, request_id: u64) {
        if !self.auth_manager.is_authorized() {
            return self.send_error_impl(request_id, 401, "Unauthorized");
        }
        if let Some(user) = self.users.get(&self.my_id).cloned() {
            return self.send_result(request_id, Object::user(user));
        }
        // The UserManager store answers first when it has the self user
        // (the M.4 surface — also the offline unit-test path); the live
        // bridge round-trip fires only on a cache miss.
        {
            let mut env = UserEnv {
                global: &mut self.global,
                options: self
                    .option_manager
                    .as_mut()
                    .expect("user requests run with the initialized option manager"),
                auth_is_bot: self.auth_manager.is_bot(),
            };
            let user_id = self.user_manager.get_me(&mut env);
            if user_id.is_valid() {
                let env = UserEnv {
                    global: &mut self.global,
                    options: self
                        .option_manager
                        .as_mut()
                        .expect("user requests run with the initialized option manager"),
                    auth_is_bot: self.auth_manager.is_bot(),
                };
                let user = self.user_manager.get_user_object_of(&env, user_id);
                return self.send_result(request_id, Object::user(user));
            }
        }
        match self.query_users_me() {
            Some(user) => {
                self.apply_self_user(&user, false);
                if let Some(user) = self.users.get(&self.my_id).cloned() {
                    self.send_result(request_id, Object::user(user));
                } else {
                    self.send_error_impl(request_id, 500, "Failed to get self user");
                }
            }
            None => {
                self.send_error_impl(request_id, 500, "Failed to get information about the user")
            }
        }
    }

    /// The text `sendMessage` (MessagesManager.cpp:21721+): the temporary
    /// local message (pending state) is answered synchronously; the wire
    /// ack (`updateShortSentMessage` or the Updates container) completes
    /// it with `updateMessageSendSucceeded`. The reply peer is a known
    /// chat (the receive path created it).
    pub(crate) fn on_send_message(&mut self, request_id: u64, args: td_api::sendMessage) {
        if !self.auth_manager.is_authorized() {
            return self.send_error_impl(request_id, 401, "Unauthorized");
        }
        if !self.chats.contains_key(&args.chat_id) {
            return self.send_error_impl(request_id, 400, "Chat not found");
        }
        let text = match args.input_message_content.as_deref() {
            Some(td_api::InputMessageContent::inputMessageText(content)) => content
                .text
                .as_ref()
                .map(|formatted| formatted.text.clone())
                .unwrap_or_default(),
            _ => {
                return self.send_error_impl(
                    request_id,
                    400,
                    "Only text messages are supported by the walking skeleton",
                );
            }
        };

        let local_id = self.next_local_message_id;
        self.next_local_message_id -= 1;
        let random_id = nonzero_secure_i64();
        let date = tdutils::time::now() as i32;
        let temp = convert::message_object(&MessageSpec {
            id: local_id,
            chat_id: args.chat_id,
            sender_user_id: self.my_id,
            date,
            text: text.clone(),
            pending: true,
            is_outgoing: true,
            content: None,
        });
        self.send_result(request_id, Object::message(temp));

        self.pending_sends.insert(
            random_id,
            PendingSend {
                local_id,
                chat_id: args.chat_id,
                text: text.clone(),
            },
        );

        // The wire query (the C++ defers it to the next actor event; the
        // observable stream is identical — the answer is already out).
        let access_hash = self.peer_access.get(&args.chat_id).copied().unwrap_or(0);
        let query =
            telegram_api::Function::messages_sendMessage(telegram_api::messages_sendMessage {
                flags: 0,
                no_webpage: false,
                silent: false,
                background: false,
                clear_draft: false,
                noforwards: false,
                update_stickersets_order: false,
                invert_media: false,
                allow_paid_floodskip: false,
                peer: Some(Box::new(telegram_api::InputPeer::inputPeerUser(
                    telegram_api::inputPeerUser {
                        user_id: args.chat_id,
                        access_hash,
                    },
                ))),
                reply_to: None,
                message: text.clone(),
                random_id,
                reply_markup: None,
                entities: Vec::new(),
                schedule_date: 0,
                schedule_repeat_period: 0,
                send_as: None,
                quick_reply_shortcut: None,
                effect: 0,
                allow_paid_stars: 0,
                rich_message: None,
                suggested_post: None,
            });
        let Some(owner) = self.bridge.as_ref() else {
            return;
        };
        let bridge = owner.handle();
        match bridge.query(query) {
            QueryAnswer::Ok(data) => {
                if let Ok(answer) = telegram_api::Updates::parse(&data) {
                    self.on_send_answer(random_id, answer);
                }
            }
            QueryAnswer::Err(code, message) => {
                self.on_send_failed(random_id, code, &message);
            }
        }
    }

    /// The `messages.sendMessage` ack: `updateShortSentMessage` (the plain
    /// text case) completes the pending send; an Updates container goes
    /// through the generic update path (its updateNewMessage carries the
    /// final message).
    fn on_send_answer(&mut self, random_id: i64, answer: telegram_api::Updates) {
        match answer {
            telegram_api::Updates::updateShortSentMessage(m) => {
                self.complete_pending_send(random_id, m.id as i64, m.date);
            }
            other => {
                // Container: process like a pushed update (the final
                // message arrives as updateNewMessage with the real id).
                let mut bytes = Vec::new();
                other.store(&mut bytes);
                self.on_net_update(bytes);
            }
        }
    }

    /// `on_send_message_success` + `send_update_message_send_succeeded`:
    /// the final message (server id/date, no sending state) replaces the
    /// temporary one.
    fn complete_pending_send(&mut self, random_id: i64, message_id: i64, date: i32) {
        let Some(pending) = self.pending_sends.remove(&random_id) else {
            return;
        };
        let final_message = convert::message_object(&MessageSpec {
            id: message_id,
            chat_id: pending.chat_id,
            sender_user_id: self.my_id,
            date,
            text: pending.text,
            pending: false,
            is_outgoing: true,
            content: None,
        });
        let old_message_id = pending.local_id;
        self.send_update(Object::updateMessageSendSucceeded(
            td_api::updateMessageSendSucceeded {
                message: Some(final_message),
                old_message_id,
            },
        ));
    }

    fn on_send_failed(&mut self, random_id: i64, code: i32, message: &str) {
        let Some(pending) = self.pending_sends.remove(&random_id) else {
            return;
        };
        let failed = convert::message_object(&MessageSpec {
            id: pending.local_id,
            chat_id: pending.chat_id,
            sender_user_id: self.my_id,
            date: tdutils::time::now() as i32,
            text: pending.text,
            pending: false,
            is_outgoing: true,
            content: None,
        });
        let old_message_id = pending.local_id;
        self.send_update(Object::updateMessageSendFailed(
            td_api::updateMessageSendFailed {
                message: Some(failed),
                old_message_id,
                error: Some(td_api::Error {
                    code,
                    message: message.to_string(),
                }),
            },
        ));
    }

    /// Creates the private chat for a peer when first needed and emits
    /// `updateNewChat` (the C++ `send_update_new_chat`).
    fn ensure_chat(&mut self, chat_id: i64) {
        if self.chats.contains_key(&chat_id) {
            return;
        }
        let user = self
            .users
            .get(&chat_id)
            .cloned()
            .unwrap_or_else(|| td_api::User {
                id: chat_id,
                ..Default::default()
            });
        let chat = convert::chat_object(&user);
        self.chats.insert(chat_id, chat.clone());
        self.send_update(Object::updateNewChat(td_api::updateNewChat {
            chat: Some(chat),
        }));
    }

    /// The bridge's raw update packets — the whole `Updates` umbrella
    /// through the M.7 engine (the S72 partial switch over the plain
    /// containers is superseded: shorts, updatesTooLong and the
    /// users/chats pre-application route through on_get_updates), then
    /// the td_api emission.
    fn on_net_update(&mut self, data: Vec<u8>) {
        let Ok(umbrella) = telegram_api::Updates::parse(&data) else {
            return;
        };
        let Some(options) = self.option_manager.as_mut() else {
            return;
        };
        let mut env = TdUpdatesEnv {
            global: &mut self.global,
            options,
            auth_is_authorized: self.auth_manager.is_authorized(),
            auth_is_bot: self.auth_manager.is_bot(),
            my_id: self.user_manager.get_my_id().get(),
            can_ignore_background_updates: self.can_ignore_background_updates,
        };
        self.updates_manager
            .on_get_updates(&mut env, umbrella, Promise::default());
        self.drain_sink();
    }

    /// Converts and emits what the sink collected.
    fn drain_sink(&mut self) {
        let sink = std::mem::take(self.updates_manager.sink());
        for user in &sink.users {
            if let telegram_api::User::user(user) = user {
                self.peer_access.insert(user.id, user.access_hash);
                let object = convert::user_object(user, user.id == self.my_id);
                let changed = self.users.get(&object.id) != Some(&object);
                self.users.insert(object.id, object.clone());
                if changed {
                    self.send_update(Object::updateUser(td_api::updateUser {
                        user: Some(object),
                    }));
                }
            }
        }
        for (message, source) in &sink.new_messages {
            self.apply_new_message(message, source);
        }
        for messages in &sink.deletions {
            self.apply_delete_messages(messages);
        }
    }

    /// The receive path for `updateDeleteMessages` (the M.10.6 delete
    /// path): the server ids run through the manager's
    /// `delete_messages_from_updates` — the rows resolve through the
    /// message-db unique-id load and the durable deletes land through the
    /// M.10.2 binlog spine. The `updateDeleteMessages` emission is the
    /// update-plane seam.
    fn apply_delete_messages(&mut self, messages: &[i32]) {
        let message_ids: Vec<crate::messages_manager::MessageId> = messages
            .iter()
            .map(|&id| {
                crate::messages_manager::MessageId::from_server_message_id(
                    crate::messages_manager::ServerMessageId::new(id),
                )
            })
            .collect();
        let mut manager = std::mem::take(&mut self.messages_manager);
        let _deleted = crate::messages_manager::delete_messages_from_updates(
            &mut self.global,
            &mut manager,
            &message_ids,
            true,
        );
        self.messages_manager = manager;
    }

    /// The receive path for a wire `updateNewMessage`/`updateNewChannelMessage`
    /// (the container users/chats were collected into the sink first): the
    /// full M.10.3 intake — `parse_telegram_api_message` → `create_message` →
    /// `fix_new_message` — runs through the M.10.2c manager, and the
    /// emitted object is the M.10.4 `get_message_object` builder. The
    /// M.9.18 flip (wire media through `get_message_content`) now lives at
    /// its C++ call position inside `parse_telegram_api_message`.
    fn apply_new_message(&mut self, message: &telegram_api::Message, source: &str) {
        // The manager and the context are both pieces of `Td`; the intake
        // is driven over a taken manager to split the borrows.
        let mut manager = std::mem::take(&mut self.messages_manager);
        let received = manager.on_get_message(self, message.clone(), true, false, source);
        self.messages_manager = manager;
        let Some((dialog_id, mut m)) = received else {
            return;
        };
        // `add_new_dialog` emits `updateNewChat` for the fresh dialog
        // (MessagesManager.cpp:33079+); the walking skeleton maps user
        // dialogs to its chat objects, the other dialog kinds ride the
        // ChatManager chat-object wiring.
        if dialog_id.get_type() == crate::messages_manager::DialogType::User {
            self.ensure_chat(dialog_id.get_user_id().get());
        }
        let object = crate::messages_manager::get_message_object(
            &self.messages_manager,
            self,
            self,
            dialog_id,
            &mut m,
            "updateNewMessage",
        );
        if let Some(object) = object {
            self.send_update(Object::updateNewMessage(td_api::updateNewMessage {
                message: Some(object),
            }));
        }
    }

    /// `Td` close: dropping the callback sends the close notification; the
    /// bridge thread stops (persisting the auth state).
    fn close(&mut self) {
        if let Some(bridge) = self.bridge.take() {
            bridge.shutdown();
        }
        drop(self.callback.take());
    }
}

/// Parses a top-level boxed `Vector<user>` (the `users.getUsers` result).
fn parse_vector_user(data: &[u8]) -> Option<Vec<telegram_api::user>> {
    let mut reader = telegram_api::tl::Reader::new(data);
    if reader.i32().ok()? != telegram_api::tl::VECTOR_ID {
        return None;
    }
    let count = reader.i32().ok()? as u32 as usize;
    let mut users = Vec::with_capacity(count);
    for _ in 0..count {
        if let telegram_api::User::user(user) = telegram_api::User::read(&mut reader).ok()? {
            users.push(user);
        }
    }
    Some(users)
}

/// A non-zero secure i64 (`MessagesManager::generate_new_random_id`).
fn nonzero_secure_i64() -> i64 {
    loop {
        let value = tdutils::random::secure_uint64() as i64;
        if value != 0 {
            return value;
        }
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
        /// A raw update packet from the net bridge (the `Updates`
        /// umbrella) — forwarded from the bridge thread over the captured
        /// `SchedulerPtr`.
        OnNetUpdate { data: Vec<u8>, } => on_net_update(data),
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

    /// The Td's single actor timeout slot multiplexes the ConfigManager
    /// expire timer, the recoverer wakeup and the UserManager deadlines
    /// (the C++ managers each carry their own timer): the fired timeout
    /// re-requests the expired config, re-runs the recoverer's loop and
    /// ticks the user timeouts; every arm/cancel point then re-arms the
    /// slot at the earliest deadline through `sync_config_timeout`.
    /// The member MultiTimeouts (the user online/emoji-status pair and the
    /// ChatManager channel trio) fire here — their C++ callbacks hop back
    /// into the manager actors; the sweep then re-arms at the next
    /// deadline, like the MultiTimeouts' set/cancel re-arm.
    fn timeout_expired(&mut self) {
        self.tick_alarms();
        let close_flag = self.close_flag != 0;
        // The NotificationManager's two timeout families (M.13): the C++
        // `on_flush_*_timeout_callback`s collapse into `tick_timeouts`.
        {
            let mut env = NotificationEnv {
                global: &mut self.global,
                options: self.option_manager.as_mut(),
                auth_is_authorized: self.auth_manager.is_authorized(),
                auth_is_bot: self.auth_manager.is_bot(),
                updates: &mut self.notification_updates,
            };
            self.notification_manager.tick_timeouts(&mut env);
            self.notification_settings_manager.tick_timeouts(&mut env);
            self.device_token_manager.tick_timeouts(&mut env);
        }
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

        // Pre-init there is no OptionManager to build the UserEnv from —
        // the manager ticks skip, but the tail flushes must still run: a
        // pre-init alarm (setAlarm is preinitialization) settles its parked
        // promise through this exact path.
        if let Some(options) = self.option_manager.as_mut() {
            let mut env = UserEnv {
                global: &mut self.global,
                options,
                auth_is_bot: self.auth_manager.is_bot(),
            };
            self.user_manager.tick_timeouts(&mut env);
            self.chat_manager
                .tick_timeouts(&mut self.user_manager, &mut env);
        }
        self.flush_user_updates();
        self.rearm_user_timeouts();

        // The update engine's timers (the gap fills, the throttled pmc
        // saves, the getDifference retry and the data reloaders; M.7.18).
        // Pre-init the ticks skip — the tail flush below must still run:
        // a pre-init alarm settles its parked promise through it.
        if let Some(options) = self.option_manager.as_mut() {
            let mut env = TdUpdatesEnv {
                global: &mut self.global,
                options,
                auth_is_authorized: self.auth_manager.is_authorized(),
                auth_is_bot: self.auth_manager.is_bot(),
                my_id: self.user_manager.get_my_id().get(),
                can_ignore_background_updates: self.can_ignore_background_updates,
            };
            self.updates_manager.tick_timeouts(&mut env);
        }
        self.flush_deferred_results();
    }

    fn tear_down(&mut self) {
        self.close();
    }

    // No `loop_`: the pending-query poll arrives with the net step (the
    // C++ `Td::loop` yields to drive the NetQueryDispatcher).
}

/// The `G()`/manager slice the collapsed NotificationManager reads through
/// (the C++ manager reads the same surfaces off its `Td *td_` directly).
/// The `send_update`s buffer into [`Td::notification_updates`] and Td
/// drains them — the collapsed `send_closure(G()->td(), &Td::send_update)`
/// hop.
pub(crate) struct NotificationEnv<'a> {
    pub(crate) global: &'a mut Global,
    /// `None` once the option manager is torn down; the getters answer
    /// their defaults, matching the C++ close ordering closely enough for
    /// the destroy path.
    pub(crate) options: Option<&'a mut OptionManager>,
    pub(crate) auth_is_authorized: bool,
    pub(crate) auth_is_bot: bool,
    pub(crate) updates: &'a mut Vec<td_api::Update>,
}

impl<'a> NotificationEnv<'a> {
    // construct via the struct literal: the fields borrow `Td` disjointly,
    // so the manager call can proceed while the env is alive
}

impl crate::notification::NotificationTypeEnv for NotificationEnv<'_> {
    // every NotificationTypeEnv surface is a seam until M.10; the defaults
    // (unloaded messages, no senders/animations) are the C++ answers for a
    // database that cannot hold those objects yet
}

impl crate::notification_manager::NotificationManagerEnv for NotificationEnv<'_> {
    /// `G()->close_flag() || !td_->auth_manager_->is_authorized() ||
    /// td_->auth_manager_->is_bot()` (NotificationManager.cpp:170-172).
    fn is_disabled(&self) -> bool {
        self.global.close_flag() || !self.auth_is_authorized || self.auth_is_bot
    }

    fn close_flag(&self) -> bool {
        self.global.close_flag()
    }

    fn get_option_integer(&self, name: &str, default_value: i64) -> i64 {
        self.options.as_ref().map_or(default_value, |options| {
            options.get_option_integer(name, default_value)
        })
    }

    fn get_option_boolean(&self, name: &str, default_value: bool) -> bool {
        self.options.as_ref().map_or(default_value, |options| {
            options.get_option_boolean(name, default_value)
        })
    }

    fn unix_time(&self) -> i32 {
        self.global.unix_time()
    }

    fn now(&self) -> f64 {
        tdutils::time::now()
    }

    fn server_time(&self) -> f64 {
        self.global.server_time()
    }

    fn send_update(&mut self, update: td_api::Update) {
        self.updates.push(update);
    }

    fn pmc_get(&mut self, key: &str) -> String {
        self.global.binlog_pmc().get(key)
    }

    fn pmc_set(&mut self, key: &str, value: &str) {
        self.global.binlog_pmc().set(key, value);
    }

    fn pmc_erase(&mut self, key: &str) {
        self.global.binlog_pmc().erase(key);
    }
}

impl crate::notification_settings_manager::NotificationSettingsManagerEnv for NotificationEnv<'_> {
    fn auth_is_authorized(&self) -> bool {
        self.auth_is_authorized
    }

    /// The C++ `was_authorized()` bands collapse into the S76
    /// `is_authorized` on this line.
    fn auth_was_authorized(&self) -> bool {
        self.auth_is_authorized
    }

    fn auth_is_bot(&self) -> bool {
        self.auth_is_bot
    }

    fn pmc_get_raw(&mut self, key: &[u8]) -> Vec<u8> {
        self.global.binlog_pmc().get_raw(key)
    }

    fn pmc_set_raw(&mut self, key: &[u8], value: &[u8]) {
        self.global.binlog_pmc().set_raw(key, value);
    }

    fn binlog_add(&mut self, handler_type: i32, data: &[u8]) -> u64 {
        self.global.td_db().binlog().add(handler_type, data)
    }
}

impl crate::device_token_manager::DeviceTokenManagerEnv for NotificationEnv<'_> {
    fn pmc_get_raw(&mut self, key: &[u8]) -> Vec<u8> {
        self.global.binlog_pmc().get_raw(key)
    }

    fn pmc_set_raw(&mut self, key: &[u8], value: &[u8]) {
        self.global.binlog_pmc().set_raw(key, value);
    }
}

/// The premium-required-to-contact QueryMerger merge function: build the
/// input users and send the merged `users.getRequirementsToContact`
/// query. The query send is [blocked: net] and answers the C++ error
/// surface until M.4.16 wires it (which then routes the answer through
/// `UserManager::on_get_is_premium_required_to_contact_users`).
struct MergePremiumRequiredContactQueries {
    user_ids: Vec<i64>,
    promise: Promise<()>,
}

impl tdactor::ActorMessage for MergePremiumRequiredContactQueries {
    type Actor = Td;

    fn dispatch(mut self, td: &mut Td) {
        let Some(options) = td.option_manager.as_mut() else {
            // the C++ TRY_STATUS_PROMISE(promise, G()->close_status())
            return self
                .promise
                .set_error(Status::error(500, "Request aborted"));
        };
        let env = UserEnv {
            global: &mut td.global,
            options,
            auth_is_bot: td.auth_manager.is_bot(),
        };
        if env.global.close_flag() {
            return self
                .promise
                .set_error(Status::error(500, "Request aborted"));
        }
        let user_ids: Vec<UserId> = self.user_ids.iter().map(|id| UserId::new(*id)).collect();
        let input_users = td.user_manager.get_input_users_force(&env, &user_ids);
        let _ = input_users; // the GetIsPremiumRequiredToContactQuery payload
        tdutils::log_info!(
            "Get requirements to contact {} users — [blocked: net]",
            user_ids.len()
        );
        self.promise
            .set_error(Status::error(500, "Network is not implemented yet"));
    }
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
    send_closure_later(
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
            Some(td_api::AuthorizationState::authorizationStateWaitTdlibParameters(_)) => {
                "waitTdlibParameters"
            }
            Some(td_api::AuthorizationState::authorizationStateWaitPhoneNumber(_)) => {
                "waitPhoneNumber"
            }
            Some(td_api::AuthorizationState::authorizationStateReady(_)) => "ready",
            Some(td_api::AuthorizationState::authorizationStateClosing(_)) => "closing",
            Some(td_api::AuthorizationState::authorizationStateClosed(_)) => "closed",
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

    /// Test-only Td message: arm a user-online deadline (the sweep-wiring
    /// fixture — production deadlines are armed by the manager calls
    /// themselves, which the net-free Td cannot yet reach).
    struct FireOnlineDeadline {
        user_id: i64,
        was_online: i32,
        deadline: f64,
    }

    impl tdactor::ActorMessage for FireOnlineDeadline {
        type Actor = Td;

        fn dispatch(self, td: &mut Td) {
            td.user_manager.seed_online_deadline_for_test(
                crate::user_manager::UserId::new(self.user_id),
                self.was_online,
                self.deadline,
            );
            td.rearm_user_timeouts();
        }
    }

    #[test]
    fn user_online_deadline_fires_through_the_td_actor() {
        let callback = CollectCallback::default();
        run_actor_with(&callback, |actor, _callback, scheduler| {
            let dir = temp_db_dir("online-deadline");
            send(
                actor,
                scheduler,
                13,
                set_tdlib_parameters(dir.clone(), false),
            );
            let send_ptr = scheduler.send_scheduler();
            {
                let _guard = send_ptr.guard(false);
                tdactor::send_closure(
                    actor,
                    FireOnlineDeadline {
                        user_id: 5,
                        was_online: tdutils::time::system() as i32 - 120,
                        deadline: tdutils::time::now() + 0.05,
                    },
                );
            }
            // The production pump pattern (`while run_main(0.01) {}`): the
            // inbound dispatch arms the deadline and yields; the next pump
            // waits for the armed actor timeout and fires the sweep.
            let _ = scheduler.run_main(0.01);
            let _ = scheduler.run_main(0.3);
        });
        let fired = callback
            .responses
            .lock()
            .unwrap()
            .iter()
            .any(|(id, object)| {
                *id == 0
                    && matches!(
                        object,
                        Object::updateUserStatus(update) if update.user_id == 5
                            && matches!(
                                update.status,
                                Some(td_api::UserStatus::userStatusOffline(_))
                            )
                    )
            });
        assert!(
            fired,
            "the expired online deadline must broadcast updateUserStatus"
        );
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
                    value: Some(td_api::OptionValue::optionValueInteger(
                        td_api::optionValueInteger { value: 59 },
                    )),
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
                    value: Some(td_api::OptionValue::optionValueBoolean(
                        td_api::optionValueBoolean { value: true },
                    )),
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
                        value: Some(td_api::OptionValue::optionValueString(
                            td_api::optionValueString {
                                value: "hello".to_string(),
                            },
                        )),
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

    /// The M.10.20 read-family flips through the real dispatcher: the
    /// local lookups over an empty manager state (the null-result 404 and
    /// the C++ "Chat not found" table).
    #[test]
    fn m10_20_read_family_arms() {
        // the deep debug frames of the history walk carry the 32 KB
        // td_api::Object — run the body on a big-stack thread
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(m10_20_read_family_arms_impl)
            .unwrap()
            .join()
            .unwrap();
    }

    fn m10_20_read_family_arms_impl() {
        let callback = CollectCallback::default();
        let dir = temp_db_dir("m10-20-reads");
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
        td.auth_manager.force_authorized_for_test();

        // getMessageLocally: no dialog → the null-result 404
        td.request(
            101,
            Function::getMessageLocally(td_api::getMessageLocally {
                chat_id: 1,
                message_id: 100,
            }),
        );
        // the null-result 404 rides the response channel (the C++
        // `Td::send_result(nullptr)` shape)
        assert!(responses_of(&callback, 101).iter().any(|object| matches!(
            object,
            Object::error(e) if e.code == 404 && e.message == "Not Found"
        )));

        // getMessageLink: no dialog → the C++ 400
        td.request(
            102,
            Function::getMessageLink(td_api::getMessageLink {
                chat_id: 1,
                message_id: 100,
                media_timestamp: 0,
                checklist_task_id: 0,
                poll_option_id: String::new(),
                for_album: false,
                in_message_thread: false,
            }),
        );
        let error = error_of(&callback, 102);
        assert_eq!(error.code, 400);
        assert_eq!(error.message, "Chat not found");

        // getMessageEmbeddingCode: the same local table
        td.request(
            103,
            Function::getMessageEmbeddingCode(td_api::getMessageEmbeddingCode {
                chat_id: 1,
                message_id: 100,
                for_album: false,
            }),
        );
        let error = error_of(&callback, 103);
        assert_eq!(error.code, 400);
        assert_eq!(error.message, "Chat not found");

        // getChatHistory: the split-context adapter — the same local table
        td.request(
            104,
            Function::getChatHistory(td_api::getChatHistory {
                chat_id: 1,
                from_message_id: 0,
                offset: 0,
                limit: 10,
                only_local: false,
            }),
        );
        let error = error_of(&callback, 104);
        assert_eq!(error.code, 400);
        assert_eq!(error.message, "Chat not found");
        // the manager survived the take/restore
        assert!(td.messages_manager.dialogs.is_empty());

        // getChatMessageByDate: same
        td.request(
            105,
            Function::getChatMessageByDate(td_api::getChatMessageByDate {
                chat_id: 1,
                date: 1700000000,
            }),
        );
        let error = error_of(&callback, 105);
        assert_eq!(error.code, 400);
        assert_eq!(error.message, "Chat not found");

        td.close_impl(false);
        let _ = std::fs::remove_dir_all(&dir);
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
                update
                    .value
                    .clone()
                    .map(|value| (update.name.clone(), value))
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
                    value: Some(td_api::OptionValue::optionValueBoolean(
                        td_api::optionValueBoolean { value: true },
                    )),
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
                        value: Some(Box::new(telegram_api::JSONValue::jsonNumber(
                            telegram_api::jsonNumber { value: 1.5 },
                        ))),
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

    /// Test-only Td message: seed the service-notifications user as `me`
    /// (the getMe fixture — production my_id restores from the binlog).
    struct SeedServiceNotificationsUser;

    impl tdactor::ActorMessage for SeedServiceNotificationsUser {
        type Actor = Td;

        fn dispatch(self, td: &mut Td) {
            // the tests exercise the authorized path (the C++ client is
            // authorized long before getMe runs)
            td.auth_manager.force_authorized_for_test();
            let mut env = UserEnv {
                global: &mut td.global,
                options: td
                    .option_manager
                    .as_mut()
                    .expect("the option manager exists after init"),
                auth_is_bot: td.auth_manager.is_bot(),
            };
            td.user_manager
                .set_my_id(&mut env, UserManager::get_service_notifications_user_id());
            td.user_manager.add_service_notifications_user(&mut env);
        }
    }

    fn seed_service_notifications_user(actor: &ActorOwn<Td>, scheduler: &mut ConcurrentScheduler) {
        let send_ptr = scheduler.send_scheduler();
        {
            let _guard = send_ptr.guard(false);
            tdactor::send_closure(actor, SeedServiceNotificationsUser);
        }
        let _ = scheduler.run_main(0.01);
    }

    fn has_answer(callback: &CollectCallback, id: u64) -> bool {
        !responses_of(callback, id).is_empty()
            || callback
                .errors
                .lock()
                .unwrap()
                .iter()
                .any(|(error_id, _)| *error_id == id)
    }

    #[test]
    fn get_me_answers_the_synthesized_service_notifications_user() {
        let callback = CollectCallback::default();
        run_actor_with(&callback, |actor, callback, scheduler| {
            let dir = temp_db_dir("m416-get-me");
            send(actor, scheduler, 1, set_tdlib_parameters(dir, false));
            seed_service_notifications_user(actor, scheduler);
            send(actor, scheduler, 2, Function::getMe(td_api::getMe));

            let responses = responses_of(callback, 2);
            assert_eq!(responses.len(), 1, "getMe answers exactly once");
            let Object::user(user) = &responses[0] else {
                panic!("getMe answers the user object, got {:?}", responses[0]);
            };
            assert_eq!(user.id, 777000);
            assert!(user.is_support, "the C++ service notifications user");
        });
    }

    fn private_chats_scope() -> td_api::NotificationSettingsScope {
        td_api::NotificationSettingsScope::notificationSettingsScopePrivateChats(
            td_api::notificationSettingsScopePrivateChats,
        )
    }

    fn scope_settings_request(mute_for: i32) -> td_api::ScopeNotificationSettings {
        td_api::ScopeNotificationSettings {
            mute_for,
            sound_id: -1,
            show_preview: true,
            use_default_mute_stories: true,
            mute_stories: false,
            story_sound_id: -1,
            show_story_poster: true,
            disable_pinned_message_notifications: false,
            disable_mention_notifications: false,
        }
    }

    #[test]
    fn notification_settings_arms_round_trip_through_the_td() {
        let callback = CollectCallback::default();
        run_actor_with(&callback, |actor, callback, scheduler| {
            let dir = temp_db_dir("m13s-settings-arms");
            send(actor, scheduler, 1, set_tdlib_parameters(dir, false));
            seed_service_notifications_user(actor, scheduler);

            // an unsynchronized scope defers into the (blocked) fetch and
            // answers nothing — the C++ deferred-promise behavior
            send(
                actor,
                scheduler,
                2,
                Function::getScopeNotificationSettings(td_api::getScopeNotificationSettings {
                    scope: Some(private_chats_scope()),
                }),
            );
            scheduler.run_main(0.05);
            assert!(
                responses_of(callback, 2).is_empty(),
                "the unsynchronized scope must stay unanswered"
            );

            send(
                actor,
                scheduler,
                3,
                Function::setScopeNotificationSettings(td_api::setScopeNotificationSettings {
                    scope: Some(private_chats_scope()),
                    notification_settings: Some(scope_settings_request(100)),
                }),
            );
            scheduler.run_main(0.05);
            let responses = responses_of(callback, 3);
            assert_eq!(responses.len(), 1);
            assert!(matches!(responses[0], Object::ok(_)), "set answers ok");

            // the synchronized scope answers the stored settings
            send(
                actor,
                scheduler,
                4,
                Function::getScopeNotificationSettings(td_api::getScopeNotificationSettings {
                    scope: Some(private_chats_scope()),
                }),
            );
            scheduler.run_main(0.05);
            let responses = responses_of(callback, 4);
            let Object::scopeNotificationSettings(settings) = &responses[0] else {
                panic!("expected scopeNotificationSettings, got {:?}", responses[0]);
            };
            assert!(
                (99..=100).contains(&settings.mute_for),
                "mute_for reflects the store"
            );

            // the reset answers ok and re-synchronizes the defaults
            send(
                actor,
                scheduler,
                5,
                Function::resetAllNotificationSettings(td_api::resetAllNotificationSettings),
            );
            scheduler.run_main(0.05);
            assert!(matches!(responses_of(callback, 5)[0], Object::ok(_)));
            send(
                actor,
                scheduler,
                6,
                Function::getScopeNotificationSettings(td_api::getScopeNotificationSettings {
                    scope: Some(private_chats_scope()),
                }),
            );
            scheduler.run_main(0.05);
            let responses = responses_of(callback, 6);
            let Object::scopeNotificationSettings(settings) = &responses[0] else {
                panic!("expected scopeNotificationSettings, got {:?}", responses[0]);
            };
            assert_eq!(settings.mute_for, 0, "the reset cleared the mute");
        });
    }

    #[test]
    fn register_device_arm_parks_until_the_ack() {
        let callback = CollectCallback::default();
        run_actor_with(&callback, |actor, callback, scheduler| {
            let dir = temp_db_dir("m13s-register-device");
            send(actor, scheduler, 1, set_tdlib_parameters(dir, false));
            seed_service_notifications_user(actor, scheduler);

            let device_token = |token: &str| {
                Some(td_api::DeviceToken::deviceTokenApplePush(
                    td_api::deviceTokenApplePush {
                        device_token: token.to_owned(),
                        is_app_sandbox: false,
                    },
                ))
            };

            send(
                actor,
                scheduler,
                2,
                Function::registerDevice(td_api::registerDevice {
                    device_token: device_token("tok"),
                    other_user_ids: Vec::new(),
                }),
            );
            scheduler.run_main(0.05);
            assert!(
                responses_of(callback, 2).is_empty(),
                "the register waits for the (blocked) server ack"
            );

            // a changed token supersedes: the parked request answers an
            // empty pushReceiverId like the C++ overwritten promise
            send(
                actor,
                scheduler,
                3,
                Function::registerDevice(td_api::registerDevice {
                    device_token: device_token("tok2"),
                    other_user_ids: Vec::new(),
                }),
            );
            scheduler.run_main(0.05);
            let responses = responses_of(callback, 2);
            assert_eq!(responses.len(), 1, "the superseded register answers empty");
            let Object::pushReceiverId(push_receiver_id) = &responses[0] else {
                panic!("expected pushReceiverId, got {:?}", responses[0]);
            };
            assert_eq!(push_receiver_id.id, 0);
            assert!(
                responses_of(callback, 3).is_empty(),
                "the new register still waits"
            );
        });
    }

    #[test]
    fn get_user_rejects_invalid_and_unknown_users() {
        let callback = CollectCallback::default();
        run_actor_with(&callback, |actor, callback, scheduler| {
            let dir = temp_db_dir("m416-get-user");
            send(actor, scheduler, 1, set_tdlib_parameters(dir, false));
            seed_service_notifications_user(actor, scheduler);

            send(
                actor,
                scheduler,
                2,
                Function::getUser(td_api::getUser {
                    user_id: 123456789012,
                }),
            );
            let error = error_of(callback, 2);
            assert_eq!(
                (error.code, error.message),
                (400, "User not found".to_string())
            );

            send(
                actor,
                scheduler,
                3,
                Function::getUser(td_api::getUser { user_id: -5 }),
            );
            let error = error_of(callback, 3);
            assert_eq!(
                (error.code, error.message),
                (400, "Invalid user identifier".to_string())
            );

            // the known special user answers from the synthesis
            send(
                actor,
                scheduler,
                4,
                Function::getUser(td_api::getUser { user_id: 777000 }),
            );
            let responses = responses_of(callback, 4);
            let Object::user(user) = &responses[0] else {
                panic!("getUser answers the user object");
            };
            assert_eq!(user.id, 777000);
        });
    }

    #[test]
    fn set_name_answers_the_local_gate_and_parks_the_query() {
        let callback = CollectCallback::default();
        run_actor_with(&callback, |actor, callback, scheduler| {
            let dir = temp_db_dir("m416-set-name");
            send(actor, scheduler, 1, set_tdlib_parameters(dir, false));
            seed_service_notifications_user(actor, scheduler);

            send(
                actor,
                scheduler,
                2,
                Function::setName(td_api::setName {
                    first_name: String::new(),
                    last_name: "x".to_string(),
                }),
            );
            let error = error_of(callback, 2);
            assert_eq!(
                (error.code, error.message),
                (400, "First name must be non-empty".to_string())
            );

            // the profile query is [blocked: net]: validated, then parked
            // without an answer (the C++ promise rides UpdateProfileQuery)
            send(
                actor,
                scheduler,
                3,
                Function::setName(td_api::setName {
                    first_name: "A".to_string(),
                    last_name: "B".to_string(),
                }),
            );
            assert!(
                !has_answer(callback, 3),
                "the profile query parks until the server ack"
            );
        });
    }

    #[test]
    fn get_contacts_parks_on_a_fresh_install_until_the_server_load() {
        let callback = CollectCallback::default();
        run_actor_with(&callback, |actor, callback, scheduler| {
            let dir = temp_db_dir("m416-get-contacts");
            send(actor, scheduler, 1, set_tdlib_parameters(dir, false));
            seed_service_notifications_user(actor, scheduler);

            // no contacts database and no saved count: the C++ parks the
            // request on the contacts.getContacts load — [blocked: net]
            // here, so the request answers nothing yet
            send(
                actor,
                scheduler,
                2,
                Function::getContacts(td_api::getContacts),
            );
            assert!(
                !has_answer(callback, 2),
                "getContacts parks until the contacts load"
            );
        });
    }

    #[test]
    fn get_close_friends_parks_until_the_contacts_load() {
        let callback = CollectCallback::default();
        run_actor_with(&callback, |actor, callback, scheduler| {
            let dir = temp_db_dir("m416-close-friends");
            send(actor, scheduler, 1, set_tdlib_parameters(dir, false));
            seed_service_notifications_user(actor, scheduler);

            // GetCloseFriendsRequest parks the actor on the contacts load
            // (the pending_user_requests re-run answers it on completion)
            send(
                actor,
                scheduler,
                2,
                Function::getCloseFriends(td_api::getCloseFriends),
            );
            assert!(
                !has_answer(callback, 2),
                "getCloseFriends parks until the contacts load"
            );
        });
    }

    #[test]
    fn get_current_state_streams_the_seeded_user_block() {
        let callback = CollectCallback::default();
        run_actor_with(&callback, |actor, callback, scheduler| {
            let dir = temp_db_dir("m416-current-state");
            send(actor, scheduler, 1, set_tdlib_parameters(dir, false));
            seed_service_notifications_user(actor, scheduler);

            send(
                actor,
                scheduler,
                2,
                Function::getCurrentState(td_api::getCurrentState),
            );
            let responses = responses_of(callback, 2);
            assert_eq!(responses.len(), 1);
            let Object::updates(updates) = &responses[0] else {
                panic!("getCurrentState answers the updates object");
            };
            assert!(
                updates.updates.iter().any(|update| matches!(
                    update,
                    td_api::Update::updateUser(u) if u.user.as_ref().is_some_and(|u| u.id == 777000)
                )),
                "the seeded user streams as updateUser"
            );
            // no user_full was loaded for `me` in this fixture: the C++
            // streams updateUserFullInfo only for the loaded users_full_ rows
            assert!(
                !updates
                    .updates
                    .iter()
                    .any(|update| matches!(update, td_api::Update::updateUserFullInfo(_))),
                "no updateUserFullInfo without a loaded user_full"
            );
        });
    }

    #[test]
    fn set_close_friends_rejects_unknown_users() {
        let callback = CollectCallback::default();
        run_actor_with(&callback, |actor, callback, scheduler| {
            let dir = temp_db_dir("m416-close-friends-gate");
            send(actor, scheduler, 1, set_tdlib_parameters(dir, false));
            seed_service_notifications_user(actor, scheduler);

            send(
                actor,
                scheduler,
                2,
                Function::setCloseFriends(td_api::setCloseFriends {
                    user_ids: vec![123456789012],
                }),
            );
            let error = error_of(callback, 2);
            assert_eq!(
                (error.code, error.message),
                (400, "User not found".to_string())
            );
        });
    }

    // ------------------------------------------------------------------
    // M.11 — the request-actor settlement machine (request_actor.rs) and
    // the AlarmManager port.
    // ------------------------------------------------------------------

    #[test]
    fn set_alarm_answers_ok_after_the_deadline() {
        let callback = CollectCallback::default();
        run_actor_with(&callback, |actor, callback, scheduler| {
            // setAlarm is a preinitialization request: it parks and fires
            // without any setTdlibParameters, like the C++ AlarmManager
            // created in `Td::start_up`.
            send(
                actor,
                scheduler,
                7,
                Function::setAlarm(td_api::setAlarm { seconds: 0.05 }),
            );
            assert!(
                responses_of(callback, 7).is_empty() && !has_answer(callback, 7),
                "the ok parks until the deadline"
            );
            let _ = scheduler.run_main(0.1);
            // the second pump: run_once skips the post-inbound wait once
            // the inbound is processed, so the armed timeout fires here
            let _ = scheduler.run_main(0.4);
            let responses = responses_of(callback, 7);
            assert_eq!(responses.len(), 1, "the alarm answers exactly once");
            assert!(matches!(responses[0], Object::ok(_)));
        });
    }

    #[test]
    fn set_alarm_rejects_out_of_range_seconds() {
        let callback = CollectCallback::default();
        run_actor_with(&callback, |actor, callback, scheduler| {
            send(
                actor,
                scheduler,
                1,
                Function::setAlarm(td_api::setAlarm { seconds: -1.0 }),
            );
            send(
                actor,
                scheduler,
                2,
                Function::setAlarm(td_api::setAlarm { seconds: 4e9 }),
            );
            for id in [1, 2] {
                let error = error_of(callback, id);
                assert_eq!(
                    (error.code, error.message),
                    (400, "Wrong parameter seconds specified".to_string())
                );
            }
        });
    }

    #[test]
    fn close_aborts_parked_requests_and_loses_their_answers() {
        // The M.11.2 close matrix: a parked request (a far-future alarm)
        // gets the clear_requests answer; the alarm teardown's own aborted
        // promise re-enters untracked and drops; a request arriving after
        // close gets the Close-state answer.
        let callback = CollectCallback::default();
        run_actor_with(&callback, |actor, callback, scheduler| {
            send(
                actor,
                scheduler,
                1,
                Function::setAlarm(td_api::setAlarm { seconds: 3600.0 }),
            );
            send(actor, scheduler, 2, Function::close(td_api::close));
            let _ = scheduler.run_main(0.1);

            let error = error_of(callback, 1);
            assert_eq!(
                (error.code, error.message),
                (500, "Request aborted".to_string())
            );
            let errors = callback
                .errors
                .lock()
                .unwrap()
                .iter()
                .filter(|(error_id, _)| *error_id == 1)
                .count();
            assert_eq!(errors, 1, "the dropped teardown error answers once only");
            assert!(matches!(responses_of(callback, 2)[0], Object::ok(_)));

            // Post-close requests: the 500 "Request aborted" Close branch.
            send(
                actor,
                scheduler,
                3,
                Function::testCallEmpty(td_api::testCallEmpty),
            );
            let _ = scheduler.run_main(0.1);
            let error = error_of(callback, 3);
            assert_eq!(
                (error.code, error.message),
                (500, "Request aborted".to_string())
            );
        });
    }

    #[test]
    fn destroy_answers_parked_requests_unauthorized() {
        // The C++ state gate special-cases only close pre-Run
        // (Td.cpp:314-319) — destroy in WaitParameters answers the 400
        // "Initialization parameters are needed" and never closes. The
        // 401 drain needs State::Run: park a preinitialization alarm,
        // init, then destroy (destroy is an authentication request,
        // Td.cpp:180, so it passes the Run gate unauthorized).
        let callback = CollectCallback::default();
        run_actor_with(&callback, |actor, callback, scheduler| {
            send(
                actor,
                scheduler,
                1,
                Function::setAlarm(td_api::setAlarm { seconds: 3600.0 }),
            );
            send(
                actor,
                scheduler,
                2,
                set_tdlib_parameters(temp_db_dir("m11-destroy-drain"), false),
            );
            seed_service_notifications_user(actor, scheduler);
            send(actor, scheduler, 3, Function::destroy(td_api::destroy));
            let _ = scheduler.run_main(0.1);
            let error = error_of(callback, 1);
            assert_eq!(
                (error.code, error.message),
                (401, "Unauthorized".to_string())
            );
            assert!(matches!(responses_of(callback, 3)[0], Object::ok(_)));
        });
    }
}
