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

use parking_lot::Mutex;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Arc;

use crate::boost_manager::{BoostManager, BoostManagerEnv};
use crate::community_manager::{CommunityManager, CommunityManagerEnv};
use crate::premium::{PremiumEnv, PremiumManager};
use crate::quick_reply_manager::{QuickReplyManager, QuickReplyManagerEnv};
use crate::statistics_manager::{StatisticsManager, StatisticsManagerEnv};
use tdactor::{Actor, ActorOwn, Scheduler, actor_messages, send_closure_later};
use tddb::td_db::{OpenedDatabase, Parameters as DbParameters, TdDb};
use tdmtproto::telegram_api;
use tdnet::net_query::{NetQuery, NetQueryDispatcher};
use tdutils::promise::Promise;
use tdutils::status::Status;

use crate::account_manager::{AccountManager, AccountManagerEnv};

use crate::auth_manager::{AuthManager, AuthQuerySender};
use crate::chat_manager::{
    ChannelId, ChannelType, ChatId, ChatManager, DcId, MessageId, RestrictedRights,
};
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
use crate::convert;
use crate::dialog_manager::DialogManager;
use crate::emoji_status::CustomEmojiId;
use crate::global::Global;
use crate::link_manager::LinkManager;
use crate::manager_option_states::ManagerOptionStates;
use crate::message_content::*;
use crate::message_entity::FormattedText;
use crate::messages_manager::{
    DialogId, EphemeralMessageFullId, FileUploadId, MessageFullId, QuickReplyMessageFullId,
    QuickReplyShortcutId, StoryFullId, StoryId,
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
use crate::user_manager::SecretChatId;
use crate::user_manager::{AccessRights, UserEnv, UserId, UserManager};
use crate::web_browser_manager::WebBrowserManager;

use crate::call_manager::CallManager;
use crate::dh_cache::DhConfig;

use crate::files::{
    DocumentByHash, DownloadCallback, DownloadCounters, DownloadManager, DownloadManagerCallback,
    DownloadManagerContext, DownloadNetMeta, DownloadQuery, DownloadQueryType, DownloadResult,
    FileData, FileDb, FileDbId, FileDownloaderNet, FileGcParameters, FileGcSink,
    FileHashUploaderNet, FileId, FileLoadManager, FileLoadManagerOptions, FileLoadNetFactory,
    FileManager, FileManagerContext, FileManagerWorker, FileNode, FileSourceId, FileStatsSink,
    FileType, FileUploaderNet, FileView, FullLocalFileLocation, StorageContext, StorageManager,
    UploadPartQuery,
};

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

/// The td-level [`UpdatesSink`] — the collector form carrying the M.10
/// receive path: the engine drives it (the users/chats pre-application
/// and the applied pts updates) and `Td::drain_sink` converts/emits
/// through the landed managers (the M.10.3 receive head, the M.10.5 edit
/// intake, the M.10.6 deletes and the M.10.7 reads).
#[derive(Default)]
pub(crate) struct TdSink {
    pub(crate) users: Vec<tdmtproto::telegram_api::User>,
    pub(crate) chats: Vec<tdmtproto::telegram_api::Chat>,
    pub(crate) new_messages: Vec<(tdmtproto::telegram_api::Message, &'static str)>,
    pub(crate) deletions: Vec<Vec<i32>>,
    /// `updateEditMessage` — (message, pts) for the M.10.5 edit intake.
    pub(crate) edits: Vec<(tdmtproto::telegram_api::Message, i32)>,
    /// `updateReadHistoryInbox` — (peer, max_id, top_msg_id) for the
    /// M.10.7 read intake.
    pub(crate) reads_inbox: Vec<(tdmtproto::telegram_api::Peer, i32, i32)>,
    /// `updateReadHistoryOutbox` — (peer, max_id).
    pub(crate) reads_outbox: Vec<(tdmtproto::telegram_api::Peer, i32)>,
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
            PendingUpdate::Wire(telegram_api::Update::updateEditMessage(update)) => {
                if let Some(message) = update.message {
                    self.edits.push((message, update.pts));
                }
            }
            PendingUpdate::Wire(telegram_api::Update::updateReadHistoryInbox(update)) => {
                if let Some(peer) = update.peer {
                    self.reads_inbox
                        .push((peer, update.max_id, update.top_msg_id));
                }
            }
            PendingUpdate::Wire(telegram_api::Update::updateReadHistoryOutbox(update)) => {
                if let Some(peer) = update.peer {
                    self.reads_outbox.push((peer, update.max_id));
                }
            }
            // `updateReadMessagesContents` (read_message_content_from_updates,
            // MessagesManager.cpp:9325) rides the TTL content-read plane and
            // `updatePinnedMessages` the M.10.14 pinned tail — both collect
            // nothing until their slices land.
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
    /// The C++ `Td::bot_info_manager_` (`make_unique` at `Td.cpp:1175`,
    /// the `init_managers` block, before the LinkManager at :1239 and the
    /// NotificationManager at :1251; `reset_manager` at Td.cpp:520) — the
    /// M.45 bot-administration query surface; its parked query promises
    /// and the bot-info fan-in live in it.
    pub(crate) bot_info_manager: crate::bot_info_manager::BotInfoManager,
    /// `Td::story_manager_` (`Td.cpp:1296`, M.47): the story state
    /// machine.
    pub(crate) story_manager: crate::story_manager::StoryManager,
    /// The story manager's `send_update` outbox — shares the notification
    /// drain (`flush_user_updates`).
    /// `Td::callback_queries_manager_` (`Td.cpp:1151`,
    /// `init_non_actor_managers` — the plain unique_ptr with no actor).
    pub(crate) callback_queries_manager: crate::callback_queries_manager::CallbackQueriesManager,
    /// `Td::bot_recommendation_manager_` (`Td.cpp:1178`,
    /// `init_managers`).
    pub(crate) bot_recommendation_manager:
        crate::bot_recommendation_manager::BotRecommendationManager,
    /// `Td::game_manager_` (`Td.cpp:1228-1230`, `init_managers`; the C++
    /// also hands the actor to `G()->set_game_manager` — the net step).
    pub(crate) game_manager: crate::game_manager::GameManager,
    /// `Td::web_app_manager_` (`Td.cpp:1329-1331`, `init_managers`; the
    /// actor id also lands in `G()->set_web_app_manager` — the net step).
    pub(crate) web_app_manager: crate::web_app_manager::WebAppManager,
    pub(crate) auth_manager: AuthManager,
    /// `Td::user_manager_` — the user/secret-chat cache (created with Td,
    /// loaded from the binlog pmc in `init` like the C++ constructor).
    pub(crate) user_manager: UserManager,
    /// `Td::message_query_manager_` (`Td.cpp:1245`, M.18) — the collapsed
    /// MessageQueryManager; created with Td like the C++ `init_managers`.
    pub(crate) message_query_manager: crate::message_query_manager::MessageQueryManager,
    /// `Td::inline_message_manager_` (Td.cpp:1234, M.21).
    pub(crate) inline_message_manager: crate::inline_message_manager::InlineMessageManager,
    /// `Td::inline_queries_manager_` (Td.cpp:1237, M.21).
    pub(crate) inline_queries_manager: crate::inline_queries_manager::InlineQueriesManager,
    /// The inline managers' `send_update` outbox — drained by
    /// [`Td::flush_user_updates`].
    pub(crate) inline_updates: Vec<td_api::Update>,
    /// The message-query manager's `send_update` outbox — the collapsed
    /// `send_closure(G()->td(), &Td::send_update, ...)` hop; drained by
    /// [`Td::flush_user_updates`].
    pub(crate) message_query_updates: Vec<td_api::Update>,
    /// `Td::notification_manager_` (`Td.cpp:1251`).
    pub(crate) notification_manager: NotificationManager,
    /// `Td::notification_settings_manager_` (`Td.cpp:1254`, M.13S).
    pub(crate) notification_settings_manager:
        crate::notification_settings_manager::NotificationSettingsManager,
    /// `Td::transcription_manager_` (`Td.cpp:1313`, M.25) — the
    /// speech-recognition query manager; the message-plane and
    /// VoiceNotes/VideoNotes callers ride the env seams until M.10/M.28.
    pub(crate) transcription_manager: crate::transcription_manager::TranscriptionManager,
    /// The transcription manager's `send_update` outbox — drained by
    /// [`Td::flush_user_updates`].
    pub(crate) transcription_updates: Vec<td_api::Update>,
    /// `Td::translation_manager_` (`Td.cpp:1316`, M.25) — the
    /// AI-compose/translation query manager; created with Td like the
    /// C++ `init_managers`.
    pub(crate) translation_manager: crate::translation_manager::TranslationManager,
    /// The translation manager's `send_update` outbox — drained by
    /// [`Td::flush_user_updates`].
    pub(crate) translation_updates: Vec<td_api::Update>,
    /// `Td::account_manager_` (`make_unique` at `Td.cpp:1231`, the
    /// `init_managers` block).
    pub(crate) account_manager: AccountManager,
    /// `Td::terms_of_service_manager_` (Td.cpp:1234).
    pub(crate) terms_of_service_manager: crate::terms_of_service_manager::TermsOfServiceManager,
    /// `Td::suggested_action_manager_` (Td.cpp:1236).
    pub(crate) suggested_action_manager: crate::suggested_action_manager::SuggestedActionManager,
    /// The account plane's `send_update` outbox; drained by
    /// [`Td::flush_user_updates`].
    pub(crate) account_updates: Vec<td_api::Update>,
    /// `Td::forum_topic_manager_` (`Td.cpp:1297`, M.49).
    pub(crate) forum_topic_manager: crate::forum_topic_manager::ForumTopicManager,
    /// `Td::secure_manager_` (`Td.cpp:1353`, `init_pure_actor_managers`)
    /// — the M.44 collapsed manager; the C++ actor's `start_up` is empty.
    pub(crate) secure_manager: crate::secure_manager::SecureManager,
    /// `Td::star_gift_manager_` (Td.cpp:1286, the `Td::Td` manager block).
    pub(crate) star_gift_manager: crate::star_gift_manager::StarGiftManager,
    /// `Td::theme_manager_` (`Td.cpp:1304`, the init managers block; the
    /// M.41 Wave-3 slice).
    pub(crate) theme_manager: crate::theme_manager::ThemeManager,
    /// The theme manager's `send_update` outbox — the same collapsed hop;
    /// drained by [`Td::flush_user_updates`].
    pub(crate) theme_updates: Vec<td_api::Update>,
    /// `Td::autosave_manager_` (`Td.cpp:1166`, the init_managers block;
    /// the M.39 Wave-3 slice).
    pub(crate) autosave_manager: crate::autosave_manager::AutosaveManager,
    /// The autosave manager's `send_update` outbox — the same collapsed
    /// hop; drained by [`Td::flush_user_updates`].
    pub(crate) autosave_updates: Vec<td_api::Update>,
    /// `Td::boost_manager_` (the C++ init_managers block).
    pub(crate) boost_manager: BoostManager,
    /// `Td::quick_reply_manager_` (the C++ init_managers block).
    pub(crate) quick_reply_manager: QuickReplyManager,
    /// The premium plane (Premium.cpp is a free-function surface).
    pub(crate) premium_manager: PremiumManager,
    /// `Td::statistics_manager_` (the C++ init_managers block).
    pub(crate) statistics_manager: StatisticsManager,
    /// The statistics plane's `send_update` outbox; drained by
    /// [`Td::flush_user_updates`].
    pub(crate) statistics_updates: Vec<td_api::Update>,
    /// `Td::community_manager_` (the C++ init_managers block).
    pub(crate) community_manager: CommunityManager,
    /// The community plane's `send_update` outbox; drained by
    /// [`Td::flush_user_updates`].
    pub(crate) community_updates: Vec<td_api::Update>,
    /// `Td::device_token_manager_` (`Td.cpp:1345`, M.13S).
    pub(crate) device_token_manager: crate::device_token_manager::DeviceTokenManager,
    /// `Td::language_pack_manager_` (`Td.cpp:1348`, M.24) — the
    /// constructor + start_up work runs in [`Td::init_language_pack_manager`]
    /// where the option manager exists.
    pub(crate) language_pack_manager: crate::language_pack_manager::LanguagePackManager,
    /// The language pack manager's `send_update` outbox — the collapsed
    /// `send_closure(G()->td(), &Td::send_update, ...)` hop; drained by
    /// [`Td::flush_user_updates`].
    pub(crate) language_pack_updates: Vec<td_api::Update>,
    /// `Td::animations_manager_` (`Td.cpp:1160`, M.20) — the constructor
    /// and start_up halves run in [`Td::init_animations_manager`] where
    /// the option manager exists.
    pub(crate) animations_manager: crate::animations_manager::AnimationsManager,
    /// The animations manager's `send_update` outbox — the collapsed
    /// `send_closure(G()->td(), &Td::send_update, ...)` hop; drained by
    /// [`Td::flush_user_updates`].
    pub(crate) animations_updates: Vec<td_api::Update>,
    /// `Td::poll_manager_` (Td.cpp:1263-1265, M.19).
    pub(crate) poll_manager: crate::poll_manager::PollManager,
    /// `Td::secret_chats_manager_` (`Td.cpp:1293`, M.50).
    pub(crate) secret_chats_manager: crate::secret_chats_manager::SecretChatsManager,
    /// `Td::group_call_manager_` (`Td.cpp:1294`, M.51).
    pub(crate) group_call_manager: crate::group_call_manager::GroupCallManager,
    /// `Td::referral_program_manager_` (`Td.cpp:1295`, M.52).
    pub(crate) referral_program_manager: crate::referral_program::ReferralProgramManager,
    /// `Td::business_manager_` (`Td.cpp:1292`, M.48).
    pub(crate) business_manager: crate::business_manager::BusinessManager,
    /// `Td::business_connection_manager_` (`Td.cpp:1291`, M.48).
    pub(crate) business_connection_manager:
        crate::business_connection_manager::BusinessConnectionManager,
    /// `Td::web_browser_manager_` (`make_unique` at `Td.cpp:1332`, the
    /// `init_managers` block).
    pub(crate) web_browser_manager: WebBrowserManager,
    /// The web browser manager's `send_update` outbox — the collapsed
    /// `send_closure(G()->td(), &Td::send_update, ...)` hop; drained by
    /// [`Td::flush_user_updates`].
    pub(crate) web_browser_updates: Vec<td_api::Update>,
    /// `Td::call_manager_` (`make_unique` at `Td.cpp:1186`, the
    /// `init_managers` block).
    pub(crate) call_manager: CallManager,
    /// The call plane's `send_update` outbox (updateCall /
    /// updateNewCallSignalingData); drained by [`Td::flush_user_updates`].
    pub(crate) call_updates: Vec<td_api::Update>,
    /// The `G()->get_dh_config()` store — the C++ keeps the DhConfig in
    /// the ConfigManager's persisted state; the collapsed Td field holds
    /// it until that surface lands (M.3 continuation).
    pub(crate) call_dh_config: Option<DhConfig>,
    /// The notification manager's `send_update` outbox — the collapsed
    /// `send_closure(G()->td(), &Td::send_update, ...)` hop; drained by
    /// [`Td::flush_user_updates`].
    pub(crate) notification_updates: Vec<td_api::Update>,
    /// `Td::web_pages_manager_` (Td.cpp:1270, M.17).
    pub(crate) web_pages_manager: crate::web_pages_manager::WebPagesManager,
    /// The web-pages pending timeout deadlines (the collapsed
    /// `pending_web_pages_timeout_` MultiTimeout; fired by
    /// `timeout_expired`).
    pub(crate) web_pages_timeouts: std::collections::HashMap<i64, f64>,
    /// The poll manager's `send_update` outbox — drained by
    /// [`Td::flush_user_updates`].
    pub(crate) poll_updates: Vec<td_api::Update>,
    /// `Td::attach_menu_manager_` (`Td.cpp:1163`, the init_managers block;
    /// the M.40 Wave-3 slice).
    pub(crate) attach_menu_manager: crate::attach_menu_manager::AttachMenuManager,
    /// The attach-menu manager's `send_update` outbox — the same collapsed
    /// hop; drained by [`Td::flush_user_updates`].
    pub(crate) attach_menu_updates: Vec<td_api::Update>,
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
    /// `Td::storage_manager_` (`Td::Td`, Td.cpp:940) — created with Td.
    pub(crate) storage_manager: StorageManager,
    /// `Td::file_manager_` (`Td::init_file_manager`, Td.cpp:1080) — behind
    /// the RefCell because the stored file contexts/worker reach it (the
    /// C++ raw `Td *td_`/`G()` hop); the Td drives borrow through the cell.
    pub(crate) file_manager: Arc<parking_lot::Mutex<FileManager>>,
    /// `Td::file_reference_manager_` (Td.cpp:1126) — the file-source
    /// registry; per-call drives only.
    pub(crate) file_reference_manager: crate::files::FileReferenceManager,
    /// The `Td::file_load_manager_` loader plane — same RefCell story as
    /// the file manager (the DownloadManager callbacks pass it into the
    /// FileManager calls).
    pub(crate) file_load_manager: Arc<parking_lot::Mutex<FileLoadManager>>,
    /// `Td::download_manager_` (Td.cpp:1219, the `init_managers` block).
    pub(crate) download_manager: Arc<parking_lot::Mutex<DownloadManager>>,
    /// The `Td *td_`/`G()` plane cell behind the stored file context/worker
    /// boxes (see [`TdFileShared`]).
    pub(crate) file_shared: Arc<parking_lot::Mutex<TdFileShared>>,
    /// `Td::parameters()->use_message_database` — the db-flag snapshot the
    /// file contexts read (set with `setTdlibParameters`).
    pub(crate) use_message_database: bool,
    /// `Td::parameters()->use_file_database`.
    pub(crate) use_file_database: bool,
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
type DeferredResults = Arc<parking_lot::Mutex<Vec<(u64, Result<Object, Status>)>>>;

impl Td {
    pub fn new(callback: Box<dyn TdCallback>) -> Self {
        // The files cluster (the C++ `Td::Td` → `init_file_manager` /
        // `init_managers` order: file_manager, file_reference_manager,
        // storage_manager, then the DownloadManager). The managers are
        // built plain, the shared `Td *td_` cell is built over their
        // handles, then the stored contexts/worker install (the boxes
        // need `'static` handles, so the cycle resolves through `Rc`).
        let file_manager = Arc::new(Mutex::new(FileManager::new()));
        let file_load_manager = Arc::new(Mutex::new(FileLoadManager::new(
            // the pre-init factory: its nets answer the session-down
            // surface; the T.9 install (init) replaces it with the
            // bridge-backed nets
            Box::new(TdFileLoadNetFactory::new()),
            FileLoadManagerOptions {
                is_premium: false,
                session_count: 1,
                keep_media_order: false,
            },
        )));
        let download_manager = Arc::new(Mutex::new(DownloadManager::new()));
        let file_shared = Arc::new(Mutex::new(TdFileShared::new(
            Arc::clone(&file_manager),
            Arc::clone(&file_load_manager),
        )));
        {
            let mut file_manager = file_manager.lock();
            file_manager.set_context(Box::new(TdFilePlane(Arc::clone(&file_shared))));
            file_manager.set_worker(Box::new(TdFileWorker {
                shared: Arc::clone(&file_shared),
                download_manager: Arc::clone(&download_manager),
            }));
            // the C++ constructor installs the `UserDownloadFileCallback`
            // (FileManager.cpp:1310) — the completion hops queue for the
            // drive-end replay into `on_user_file_download_finished`
            file_manager.set_user_download_callback(Arc::new(Mutex::new(TdUserDownloadCallback {
                events: Arc::clone(&file_shared.lock().user_download_finished_events),
            })));
        }
        {
            let mut download_manager = download_manager.lock();
            download_manager.set_callback(Box::new(TdFilePlane(Arc::clone(&file_shared))));
            download_manager.set_context(Box::new(TdFilePlane(Arc::clone(&file_shared))));
        }
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
            sent_net_queries: Arc::new(std::sync::Mutex::new(Vec::new())),
            completed_config_queries: Arc::new(std::sync::Mutex::new(Vec::new())),
            config_recoverer: None,
            pending_recoverer_fetches: Vec::new(),
            recoverer_wakeup_at: 0.0,
            #[cfg(test)]
            recoverer_clock: None,
            global: Global::new(),
            time_zone_manager: TimeZoneManager::default(),
            manager_states: ManagerOptionStates::default(),
            link_manager: None,
            bot_info_manager: crate::bot_info_manager::BotInfoManager::new(),
            story_manager: crate::story_manager::StoryManager::new(),
            callback_queries_manager: crate::callback_queries_manager::CallbackQueriesManager::new(
            ),
            bot_recommendation_manager:
                crate::bot_recommendation_manager::BotRecommendationManager::new(),
            game_manager: crate::game_manager::GameManager::new(),
            web_app_manager: crate::web_app_manager::WebAppManager::new(),
            auth_manager: AuthManager::new(),
            user_manager: UserManager::new(),
            message_query_manager: crate::message_query_manager::MessageQueryManager::new(),
            inline_message_manager: crate::inline_message_manager::InlineMessageManager::new(),
            inline_queries_manager: crate::inline_queries_manager::InlineQueriesManager::new(),
            inline_updates: Vec::new(),
            message_query_updates: Vec::new(),
            notification_manager: NotificationManager::default(),
            notification_settings_manager:
                crate::notification_settings_manager::NotificationSettingsManager::new(),
            transcription_manager: crate::transcription_manager::TranscriptionManager::default(),
            transcription_updates: Vec::new(),
            translation_manager: crate::translation_manager::TranslationManager::default(),
            translation_updates: Vec::new(),
            account_manager: AccountManager::new(),
            terms_of_service_manager: crate::terms_of_service_manager::TermsOfServiceManager::new(),
            suggested_action_manager: crate::suggested_action_manager::SuggestedActionManager::new(
            ),
            account_updates: Vec::new(),
            forum_topic_manager: crate::forum_topic_manager::ForumTopicManager::new(),
            secure_manager: crate::secure_manager::SecureManager::new(),
            star_gift_manager: crate::star_gift_manager::StarGiftManager::new(),
            theme_manager: crate::theme_manager::ThemeManager::new(),
            theme_updates: Vec::new(),
            autosave_manager: crate::autosave_manager::AutosaveManager::new(),
            autosave_updates: Vec::new(),
            boost_manager: BoostManager::new(),
            quick_reply_manager: QuickReplyManager::new(),
            premium_manager: PremiumManager::new(),
            statistics_manager: StatisticsManager::new(),
            statistics_updates: Vec::new(),
            community_manager: CommunityManager::new(),
            community_updates: Vec::new(),
            device_token_manager: crate::device_token_manager::DeviceTokenManager::new(),
            language_pack_manager: crate::language_pack_manager::LanguagePackManager::new(),
            language_pack_updates: Vec::new(),
            animations_manager: crate::animations_manager::AnimationsManager::new(),
            animations_updates: Vec::new(),
            poll_manager: crate::poll_manager::PollManager::new(),
            poll_updates: Vec::new(),
            secret_chats_manager: crate::secret_chats_manager::SecretChatsManager::new(true),
            group_call_manager: crate::group_call_manager::GroupCallManager::new(),
            referral_program_manager: crate::referral_program::ReferralProgramManager::new(),
            business_manager: crate::business_manager::BusinessManager::new(),
            business_connection_manager:
                crate::business_connection_manager::BusinessConnectionManager::new(),
            web_browser_manager: WebBrowserManager::new(),
            web_browser_updates: Vec::new(),
            call_manager: CallManager::new(),
            call_updates: Vec::new(),
            call_dh_config: None,
            notification_updates: Vec::new(),
            web_pages_manager: crate::web_pages_manager::WebPagesManager::new(),
            web_pages_timeouts: std::collections::HashMap::new(),
            attach_menu_manager: crate::attach_menu_manager::AttachMenuManager::new(),
            attach_menu_updates: Vec::new(),
            premium_required_contact_merger: None,
            chat_manager: crate::chat_manager::ChatManager::new(),
            dialog_manager: DialogManager::new(),
            updates_manager: UpdatesManager::new(TdSink::default()),
            storage_manager: StorageManager::default(),
            file_manager,
            file_reference_manager: crate::files::FileReferenceManager::new(),
            file_load_manager,
            download_manager,
            file_shared,
            use_message_database: false,
            use_file_database: false,
            messages_manager: crate::messages_manager::MessagesManager::new(),
            stop_cnt: 2,
            deferred_results: Arc::new(Mutex::new(Vec::new())),
            bridge: None,
            my_id: 0,
            my_access_hash: 0,
            users: HashMap::new(),
            chats: HashMap::new(),
            peer_access: HashMap::new(),
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
        self.use_message_database = db_parameters.use_message_database;
        self.use_file_database = db_parameters.use_file_database;
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

        // `AnimationsManager::AnimationsManager` + `start_up`
        // (Td.cpp:1160, the init_managers block) — the constructor erases
        // the legacy limit key and loads the saved `ans` record (M.20).
        self.init_animations_manager();

        // `Td::init_pure_actor_managers` (Td.cpp:1348): the
        // LanguagePackManager actor constructor + start_up halves read
        // the restored options and open the language pack database (M.24).
        self.init_language_pack_manager();

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
            self.init_attach_menu_manager();
            self.init_terms_of_service_manager();
            self.on_web_browser_authorization_success();
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
                    self.init_attach_menu_manager();
                    self.init_terms_of_service_manager();
                    self.on_web_browser_authorization_success();
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

        // The M.3.9.2 stand-in for the manager constructors' pmc reads (in
        // the C++ creation order: AccountManager Td.cpp:1157,
        // GroupCallManager :1231, UserManager :1322) — the states replay
        // from the binlog pmc and send the same updates the C++
        // constructors do. Each read moves into its manager when that
        // manager lands.
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

        // `make_unique<TranscriptionManager>` + the constructor
        // `load_trial_parameters` (Td.cpp:1313,
        // TranscriptionManager.cpp:90-95/124-142) — the M.25 move-in from
        // the M.3.9.2 stand-in. The read runs after the combined
        // M.3.9.2 block, keeping the C++ creation order of the reads it
        // covers (AccountManager :1157, GroupCallManager :1231, then
        // TranscriptionManager :1313; the UserManager :1322 freeze
        // restore collapses into the earlier `init_freeze_state`). The
        // gate is inside (the C++ load guards is_authorized && !is_bot).
        {
            let mut env = NotificationEnv {
                global: &mut self.global,
                options: self.option_manager.as_mut(),
                auth_is_authorized: self.auth_manager.is_authorized(),
                auth_is_bot: self.auth_manager.is_bot(),
                updates: &mut self.transcription_updates,
            };
            self.transcription_manager.init(&mut env);
        }

        // `make_unique<TranslationManager>` + the constructor
        // `ai_compose_tones` restore (Td.cpp:1316,
        // TranslationManager.cpp:573-592) — the M.25 partner manager;
        // the read runs right after the transcription manager's, in the
        // C++ creation order (:1313 → :1316). The gate is inside
        // (start_up guards is_authorized && !is_bot); the empty-cache
        // reload is the [blocked: net] tail.
        {
            let mut env = TranslationEnv {
                global: &mut self.global,
                options: self.option_manager.as_mut(),
                auth_is_authorized: self.auth_manager.is_authorized(),
                auth_is_bot: self.auth_manager.is_bot(),
                updates: &mut self.translation_updates,
                users: &mut self.user_manager,
            };
            self.translation_manager.init(&mut env);
        }

        // `make_unique<LinkManager>` (Td.cpp:1239, the init_managers
        // block) — the autologin-token slice (M.3.9.1); the link surface
        // is Wave 2.        // `make_unique<BotInfoManager>` (Td.cpp:1175-1177, the
        // init_managers block) — created with the manager set at Td
        // construction in the collapsed init (the field initializer); the
        // M.45 bot query surface drives it per call.
        let _ = &self.bot_info_manager;

        // `make_unique<LinkManager>` (Td.cpp:1239, the init_managers
        // block) — the autologin-token slice (M.3.9.1); the link surface
        // is Wave 2.
        self.link_manager = Some(LinkManager::new());
        // `LinkManager::start_up` (LinkManager.cpp:1895-1908) — the
        // collapsed actor start restores the three pmc domain lists (M.23).
        {
            let link_manager = self.link_manager.as_mut().expect("link manager just set");
            let mut env = TdLinkEnv {
                global: &mut self.global,
                options: self.option_manager.as_mut(),
            };
            link_manager.start_up(&mut env);
        }

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

        // The db-dependent half of the files init: the C++ contexts read
        // `G()->td_db()` lazily at first use; the snapshot refreshes here,
        // the bad-paths re-seed runs through the context re-install, and
        // the DownloadManager binlog plane loads (the counters + the
        // `dlds#` prefix read).
        self.refresh_file_shared();
        self.file_manager
            .lock()
            .set_context(Box::new(TdFilePlane(Arc::clone(&self.file_shared))));
        let dlds_records = self.global.binlog_pmc().prefix_get("dlds");
        self.file_shared.lock().dlds_preload(dlds_records);
        // the live nets install (T.9): the bridge handle + the shared
        // snapshot fill the factory cell, and the file-name counters
        // preload from the pmc (FileLoaderUtils.cpp:85-92, 116-122)
        let bridge_handle = self.bridge.as_ref().map(NetBridge::handle);
        let tmp_file_id = self
            .global
            .binlog_pmc()
            .get("tmp_file_id")
            .parse::<i32>()
            .unwrap_or(0);
        let perm_file_id = self
            .global
            .binlog_pmc()
            .get("perm_file_id")
            .parse::<i32>()
            .unwrap_or(0);
        self.file_shared
            .lock()
            .preload_file_name_counters(tmp_file_id, perm_file_id);
        if let Some(bridge_handle) = bridge_handle {
            let live_factory = TdFileLoadNetFactory::new();
            live_factory.install(bridge_handle, Arc::clone(&self.file_shared));
            self.file_load_manager
                .lock()
                .set_factory(Box::new(live_factory));
        }
        // T.7: the dlds records now ride the versioned
        // `FileManager::store_file` layout (byte-compatible with the C++
        // pmc), so the flush gate opens — the writes reach the real pmc
        self.file_shared.lock().dlds_pmc_live = true;
        self.download_manager.lock().init();
        self.download_manager.lock().load_database_files();
        // `StorageManager::start_up` (StorageManager.cpp:38-43): the
        // fast-stat and last-gc-timestamp pmc loads
        {
            let mut env = TdStorageEnv {
                shared: Arc::clone(&self.file_shared),
                global: &mut self.global,
                language_pack_database_path: self
                    .option_manager
                    .as_ref()
                    .map(|options| options.get_option_string("language_pack_database_path", ""))
                    .unwrap_or_default(),
            };
            self.storage_manager.load_fast_stat(&mut env);
            self.storage_manager.load_last_gc_timestamp(&mut env);
        }

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

        self.init_attach_menu_manager();
        // The AttachMenuManager's reload timeout (M.40): the C++ actor
        // timer collapses into the shared slot's fire.
        {
            let mut env = AttachMenuEnv {
                global: &mut self.global,
                options: self.option_manager.as_mut(),
                auth_is_authorized: self.auth_manager.is_authorized(),
                auth_is_bot: self.auth_manager.is_bot(),
                user_manager: &mut self.user_manager,
                file_manager: std::sync::Arc::clone(&self.file_manager),
                file_reference_manager: &mut self.file_reference_manager,
                updates: &mut self.attach_menu_updates,
            };
            self.attach_menu_manager.tick_timeouts(&mut env);
        }

        // `AccountManager::start_up` + `SuggestedActionManager::start_up`
        // (the C++ actors start in the init_managers block) — the pmc
        // loads; the unconfirmed-session timeout is the recorded actor
        // collapse.
        {
            let mut env = AccountEnv {
                global: &mut self.global,
                options: self.option_manager.as_mut(),
                user_manager: &mut self.user_manager,
                device_token_manager: &mut self.device_token_manager,
                auth_is_bot: self.auth_manager.is_bot(),
                updates: &mut self.account_updates,
            };
            self.account_manager.start_up(&mut env);
            self.suggested_action_manager.start_up(&mut env);
        }

        // `make_unique<PeopleNearbyManager>` (Td.cpp:1258, the
        // init_managers block) — the actor collapses to its constructor
        // side effect (see crate::people_nearby_manager).
        {
            let is_bot = self.auth_manager.is_bot();
            crate::people_nearby_manager::init(&mut PeopleNearbyEnv {
                is_bot,
                global: &mut self.global,
            });
        }

        // `make_unique<WebBrowserManager>` (Td.cpp:1332, the init_managers
        // block) + its `start_up` (WebBrowserManager.cpp:142-144) — loads
        // the saved settings only while authorized and non-bot.
        self.init_web_browser_manager();

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

        // The files cluster's hangup half (the C++ resets the manager
        // actors in `Td::clear`, Td.cpp:707-712): the stored planes drop
        // so the shared-cell cycle cannot outlive the Td.
        self.file_manager.lock().teardown();
        self.download_manager.lock().teardown();

        self.global.close_all(self.destroy_flag);
        self.option_manager = None;
        // The C++ close clears the ConnectionCreator early
        // (`G()->set_connection_creator({})`, Td.cpp:690) and the
        // LinkManager with the managers (`reset_manager(link_manager_)`,
        // Td.cpp:547) — the component slices drop with them.
        self.connection_creator = None;
        self.link_manager = None;
        self.connection_creator = None;
        self.link_manager = None;
        // `reset_manager(bot_info_manager_, "BotInfoManager")`
        // (Td.cpp:520): the manager's `hangup` fails the pending bot-info
        // fan-ins and the parked query promises with
        // `Global::request_aborted_error()` (BotInfoManager.cpp:784-796)
        // before the reset drops the state.
        self.bot_info_manager.hangup();
        self.bot_info_manager = crate::bot_info_manager::BotInfoManager::new();
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
        // The `OptionManager::option_changed` animations fan-outs
        // (OptionManager.cpp:603-608, 734-736): the search-parameter and
        // saved-animations-limit options are config-applied only, and the
        // manager re-reads + dedupes them per call, so the unconditional
        // per-apply fire is observationally the C++ changed-only path.
        {
            let mut env = TdAnimationsEnv {
                global: &mut self.global,
                options: self.option_manager.as_mut(),
                auth_is_bot: self.auth_manager.is_bot(),
                file_manager: Arc::clone(&self.file_manager),
                file_reference_manager: &mut self.file_reference_manager,
                updates: &mut self.animations_updates,
            };
            self.animations_manager
                .on_update_animation_search_emojis(&mut env);
            self.animations_manager
                .on_update_animation_search_provider(&mut env);
            self.animations_manager
                .on_update_saved_animations_limit(&mut env);
        }
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
            for update in actions.manager_updates {
                // the TranscriptionManager half moved to its manager
                // (M.25); the C++ ConfigManager.cpp:2050-2052 fan-out
                let sent = match update {
                    crate::manager_option_states::ManagerUpdate::TrialParameters {
                        weekly_number,
                        duration_max,
                        cooldown_until,
                    } => {
                        let mut env = NotificationEnv {
                            global: &mut self.global,
                            options: self.option_manager.as_mut(),
                            auth_is_authorized: self.auth_manager.is_authorized(),
                            auth_is_bot: self.auth_manager.is_bot(),
                            updates: &mut self.transcription_updates,
                        };
                        self.transcription_manager.on_update_trial_parameters(
                            &mut env,
                            weekly_number,
                            duration_max,
                            cooldown_until,
                        );
                        None
                    }
                    update => {
                        let pmc = self.global.binlog_pmc();
                        self.manager_states.on_manager_update(pmc, update)
                    }
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
        // `LinkManager::update_autologin_domains` (ConfigManager.cpp:2047)
        // — the three-list fan-out (M.23).
        if let Some((autologin_domains, url_auth_domains, whitelisted_domains)) =
            &actions.autologin_domains
            && let Some(link_manager) = self.link_manager.as_mut()
        {
            let mut env = TdLinkEnv {
                global: &mut self.global,
                options: self.option_manager.as_mut(),
            };
            link_manager.update_autologin_domains(
                &mut env,
                autologin_domains.clone(),
                url_auth_domains.clone(),
                whitelisted_domains.clone(),
            );
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
        // The MessageQueryManager's view-metrics and reactions-reload-retry
        // deadlines share the same slot (M.18): the fired timeout runs
        // `tick_timeouts` (the C++ MultiTimeout + SleepActor stand-ins).
        if let Some(query_deadline) = self.message_query_manager.next_deadline()
            && (expire_time == 0.0 || query_deadline < expire_time)
        {
            expire_time = query_deadline;
        }
        // The AlarmManager timers share the same slot (M.11.4): the
        // earliest armed alarm wins.
        if let Some(alarm_deadline) = self.next_alarm_deadline()
            && (expire_time == 0.0 || alarm_deadline < expire_time)
        {
            expire_time = alarm_deadline;
        }

        // The AttachMenuManager's reload timeout joins the same slot (M.40).
        if let Some(attach_menu_deadline) = self.attach_menu_manager.next_deadline()
            && (expire_time == 0.0 || attach_menu_deadline < expire_time)
        {
            expire_time = attach_menu_deadline;
        }
        // The UpdatesManager's gap/save/retry/reload timers join the same
        // slot (M.7.18).
        if let Some(updates_deadline) = self.updates_manager.next_deadline()
            && (expire_time == 0.0 || updates_deadline < expire_time)
        {
            expire_time = updates_deadline;
        }
        // The StorageManager's auto-gc window joins it too (M.12.10 T.8):
        // the fire re-checks `auto_gc_due` like the C++ actor timeout.
        if let Some(storage_gc_deadline) = self.next_storage_gc_deadline()
            && (expire_time == 0.0 || storage_gc_deadline < expire_time)
        {
            expire_time = storage_gc_deadline;
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
    /// The per-drive [`TdFileShared`] snapshot refresh (the C++ contexts
    /// read the same surfaces off `Td*`/`G()` at call time).
    pub(crate) fn refresh_file_shared(&mut self) {
        let is_bot = self.auth_manager.is_bot();
        let mut files_dir = String::new();
        let mut secure_files_dir = String::new();
        let mut use_storage_optimizer = false;
        let mut ignore_inline_thumbnails = false;
        let mut option_integers = BTreeMap::new();
        let mut database_paths = Vec::new();
        if self.global.has_db() {
            let (files_directory, database_directory) = {
                let db = self.global.td_db();
                database_paths = db.db_paths();
                (
                    db.parameters().files_directory.clone(),
                    db.parameters().database_directory.clone(),
                )
            };
            // `G()->get_files_dir()`/`get_secure_files_dir()`
            // (Global.cpp:149-155): the TdDb parameters, with the
            // store-all flag choosing the secure base
            files_dir = files_directory;
            let store_all = self
                .option_manager
                .as_ref()
                .map(|options| {
                    options.get_option_boolean("store_all_files_in_files_directory", false)
                })
                .unwrap_or(false);
            secure_files_dir = if store_all {
                files_dir.clone()
            } else {
                database_directory
            };
        }
        if let Some(options) = self.option_manager.as_ref() {
            use_storage_optimizer = options.get_option_boolean("use_storage_optimizer", false);
            ignore_inline_thumbnails =
                options.get_option_boolean("ignore_inline_thumbnails", false);
            for name in [
                "video_note_size_max",
                "storage_max_files_size",
                "storage_max_time_from_last_access",
                "storage_max_file_count",
                "storage_immunity_delay",
                "webfile_dc_id",
            ] {
                option_integers.insert(name.to_owned(), options.get_option_integer(name, 0));
            }
        }
        let ignore_file_names = self
            .option_manager
            .as_ref()
            .map(|options| options.get_option_boolean("ignore_file_names", false))
            .unwrap_or(false);
        self.file_shared.lock().refresh(TdFileSnapshot {
            files_dir,
            secure_files_dir,
            is_bot,
            use_message_database: self.use_message_database,
            use_file_database: self.use_file_database,
            use_storage_optimizer,
            ignore_inline_thumbnails,
            option_integers,
            unix_time: self.global.unix_time(),
            database_paths,
            is_test_dc: self.global.is_test_dc(),
            ignore_file_names,
        });
    }

    /// The files drive — the collapsed files-actor pump: refresh the
    /// shared snapshot, route the loader events into the FileManager,
    /// drain the outboxes, tick the DownloadManager counters. Runs once
    /// per actor turn (from [`Td::flush_user_updates`]), like the C++
    /// `send_closure` hops.
    pub(crate) fn drive_files(&mut self) {
        if self.state != State::Run {
            return;
        }
        self.refresh_file_shared();
        for _round in 0..64u32 {
            {
                let mut file_load_manager = self.file_load_manager.lock();
                file_load_manager.drive();
                if !file_load_manager.has_queued_events()
                    && !self.file_manager.lock().generate_has_events()
                {
                    // the generate callbacks queue independently of the
                    // loader (the external generation finishes with no
                    // loader events in flight) — the apply round must
                    // still drain them
                    break;
                }
            }
            let mut file_manager = self.file_manager.lock();
            let mut file_load_manager = self.file_load_manager.lock();
            file_manager.apply_load_events(&mut file_load_manager);
        }
        self.drain_file_shared();
        let now = self.global.unix_time();
        self.download_manager.lock().tick(f64::from(now));
    }

    /// The `StorageManager::timeout_expired` collapse (T.8,
    /// StorageManager.cpp:348-363): when the auto-gc window opens, run
    /// the gc with the default parameters and save the timestamp unless
    /// the pass aborted (the C++ skips the save on the 500 cancel).
    fn tick_storage_gc(&mut self) {
        let mut env = storage_env_fields(
            &mut self.global,
            &self.file_shared,
            self.option_manager.as_ref(),
        );
        let now = tdutils::time::system();
        if !self.storage_manager.auto_gc_due(&mut env, now) {
            return;
        }
        let parameters = FileGcParameters::from_options(&mut env, -1, -1, -1, -1);
        let mut gc_sink = TdFileGcSink(Arc::clone(&self.file_shared));
        let outcome = self
            .storage_manager
            .run_gc(&mut env, &mut gc_sink, parameters, false);
        // do not save the garbage collection timestamp if the request was
        // canceled (the 500 abort)
        if !matches!(&outcome, Err(status) if status.code() == 500) {
            self.storage_manager.save_last_gc_timestamp(&mut env, now);
        }
    }

    /// The StorageManager's deadline for the shared timeout slot (the C++
    /// `schedule_next_gc` arms its own actor timeout; the collapse parks
    /// at the window's upper bound and re-checks on fire).
    fn next_storage_gc_deadline(&mut self) -> Option<f64> {
        let use_storage_optimizer = self.file_shared.lock().use_storage_optimizer();
        self.storage_manager.next_gc_at(
            use_storage_optimizer,
            tdutils::time::system() as u32,
            tdutils::time::now(),
        )
    }

    /// The `getStorageStatistics` body over the storage env
    /// (Requests.cpp:2968-2979).
    pub(crate) fn get_storage_stats_object(
        &mut self,
        need_all_files: bool,
        dialog_limit: i32,
    ) -> td_api::StorageStatistics {
        let mut env = storage_env_fields(
            &mut self.global,
            &self.file_shared,
            self.option_manager.as_ref(),
        );
        self.storage_manager
            .get_storage_stats(&mut env, need_all_files, dialog_limit)
            .get_storage_statistics_object()
    }

    /// The `getStorageStatisticsFast` body (Requests.cpp:2981-2992).
    pub(crate) fn get_storage_stats_fast_object(&mut self) -> td_api::StorageStatisticsFast {
        let mut env = storage_env_fields(
            &mut self.global,
            &self.file_shared,
            self.option_manager.as_ref(),
        );
        self.storage_manager
            .get_storage_stats_fast(&mut env)
            .get_storage_statistics_fast_object()
    }

    /// The `getDatabaseStatistics` body (Requests.cpp:2994-3004).
    pub(crate) fn get_database_statistics_object(
        &mut self,
    ) -> Result<td_api::DatabaseStatistics, tdutils::status::Status> {
        let mut env = storage_env_fields(
            &mut self.global,
            &self.file_shared,
            self.option_manager.as_ref(),
        );
        self.storage_manager
            .get_database_stats(&mut env)
            .map(|stats| stats.get_database_statistics_object())
    }

    /// The `optimizeStorage` body (Requests.cpp:3006-3033): the
    /// `FileGcParameters` construction (the -1 sentinels read the
    /// `storage_*` options through the env) and the gc pass.
    pub(crate) fn optimize_storage(
        &mut self,
        request: td_api::optimizeStorage,
    ) -> Result<td_api::StorageStatistics, tdutils::status::Status> {
        let td_api::optimizeStorage {
            size,
            ttl,
            count,
            immunity_delay,
            file_types,
            chat_ids,
            exclude_chat_ids,
            return_deleted_file_statistics,
            chat_limit,
        } = request;
        let mut env = storage_env_fields(
            &mut self.global,
            &self.file_shared,
            self.option_manager.as_ref(),
        );
        let mut parameters =
            FileGcParameters::from_options(&mut env, size, ttl, count, immunity_delay);
        // the C++ rejects a null vector entry (Requests.cpp:3010-3012);
        // the generated model has no null FileType, so every entry maps
        for file_type in &file_types {
            parameters
                .file_types
                .push(crate::files::get_file_type(file_type));
        }
        parameters.owner_dialog_ids = chat_ids
            .iter()
            .map(|&chat_id| DialogId::new(chat_id))
            .collect();
        parameters.exclude_owner_dialog_ids = exclude_chat_ids
            .iter()
            .map(|&chat_id| DialogId::new(chat_id))
            .collect();
        parameters.dialog_limit = chat_limit;
        let mut gc_sink = TdFileGcSink(Arc::clone(&self.file_shared));
        self.storage_manager
            .run_gc(
                &mut env,
                &mut gc_sink,
                parameters,
                return_deleted_file_statistics,
            )
            .map(|stats| stats.get_storage_statistics_object())
    }

    /// The post-drive outbox drain — the `send_closure` hops land here,
    /// once per drive, in emission order.
    fn drain_file_shared(&mut self) {
        let updates = std::mem::take(&mut self.file_shared.lock().updates);
        for update in updates {
            self.send_update(update.into());
        }
        let new_file_events = std::mem::take(&mut self.file_shared.lock().new_file_events);
        for (size, real_size, cnt) in new_file_events {
            self.storage_manager.on_new_file(size, real_size, cnt);
        }
        let state_events =
            std::mem::take(&mut *self.file_shared.lock().download_state_events.lock());
        for (file_id, is_paused) in state_events {
            let (local_total_size, size, expected_size) = self
                .file_manager
                .lock()
                .get_file_view(file_id)
                .map(|view| {
                    (
                        view.local_total_size(),
                        view.size(),
                        view.expected_size(false),
                    )
                })
                .unwrap_or((0, 0, 0));
            // link_token 0: the C++ passes the actor token; the collapse
            // has no token-retry plane yet
            self.download_manager.lock().update_file_download_state(
                file_id,
                local_total_size,
                size,
                expected_size,
                is_paused,
                0,
            );
        }
        let user_download_finished =
            std::mem::take(&mut *self.file_shared.lock().user_download_finished_events.lock());
        for file_id in user_download_finished {
            self.file_manager
                .lock()
                .on_user_file_download_finished(file_id);
        }
        let worker_events = std::mem::take(&mut self.file_shared.lock().worker_events);
        let mut merge_events = Vec::new();
        if !worker_events.is_empty() {
            let mut file_manager = self.file_manager.lock();
            let mut file_load_manager = self.file_load_manager.lock();
            for event in worker_events {
                match event {
                    FileWorkerEvent::CancelDownload(node) => {
                        file_manager.replay_cancel_download(&node, &mut file_load_manager);
                    }
                    FileWorkerEvent::CancelUpload(node) => {
                        file_manager.replay_cancel_upload(&node, &mut file_load_manager);
                    }
                    FileWorkerEvent::CancelGenerate(node) => {
                        file_manager.replay_cancel_generate(&node, &mut file_load_manager);
                    }
                    FileWorkerEvent::FinishDownload(file_id) => {
                        file_manager.replay_finish_downloads(file_id);
                    }
                    FileWorkerEvent::FinishUpload(file_id) => {
                        file_manager.replay_finish_uploads(file_id, &mut file_load_manager);
                    }
                    FileWorkerEvent::RunNode(node) => {
                        file_manager.replay_run_node(&node, &mut file_load_manager);
                    }
                    FileWorkerEvent::MergeFiles(from_file_id, to_file_id) => {
                        merge_events.push((from_file_id, to_file_id));
                    }
                }
            }
        }
        for (from_file_id, to_file_id) in merge_events {
            self.file_reference_manager.merge(to_file_id, from_file_id);
        }
        let db_events = std::mem::take(&mut self.file_shared.lock().file_db_events);
        for event in db_events {
            self.apply_file_db_event(event);
        }
        self.flush_dlds();
    }

    /// The file-db replay: build the node's `FileData` (the C++
    /// `try_flush_node_info`/`try_flush_node_full` bodies) and run it
    /// through the `FileDb` gate over TdDb's "files" kv.
    fn apply_file_db_event(&mut self, event: FileDbEvent) {
        match event {
            FileDbEvent::Flush {
                node,
                other_pmc_id,
                new_remote,
                new_local,
                new_generate,
            } => {
                let data = self.node_file_data(&node);
                let mut file_db = FileDb::new(self.global.td_db().files_kv());
                file_db.set_file_data(
                    self.global.td_db().files_kv(),
                    node.pmc_id(),
                    &data,
                    new_remote,
                    new_local,
                    new_generate,
                    &mut self.file_reference_manager,
                );
                // the merge-counter drop of `try_flush_node_full`
                if other_pmc_id.is_valid() && other_pmc_id != node.pmc_id() {
                    file_db.clear_file_data(self.global.td_db().files_kv(), other_pmc_id, &data);
                }
            }
            FileDbEvent::Clear { node } => {
                let data = self.node_file_data(&node);
                let mut file_db = FileDb::new(self.global.td_db().files_kv());
                file_db.clear_file_data(self.global.td_db().files_kv(), node.pmc_id(), &data);
            }
        }
    }

    /// The `FileData` construction from a node (`try_flush_node_info`,
    /// FileManager.cpp) — the file sources ride the
    /// `FileReferenceManager` (its `FileSourceStorage` impl).
    fn node_file_data(&self, node: &FileNode) -> FileData {
        node.to_file_data(
            self.file_reference_manager
                .get_some_file_sources(node.main_file_id()),
        )
    }

    /// The `dlds#` pmc flush — gated on the T.7 record-layout unification
    /// (the D1 warning: no raw-i32 records reach the real pmc).
    fn flush_dlds(&mut self) {
        if !self.file_shared.lock().dlds_pmc_live {
            return;
        }
        let (current, saved) = {
            let shared = self.file_shared.lock();
            (shared.dlds.clone(), shared.dlds_saved.clone())
        };
        let keys: BTreeSet<_> = saved.keys().chain(current.keys()).collect();
        {
            let pmc = self.global.binlog_pmc();
            for key in keys {
                match (current.get(key), saved.get(key)) {
                    (Some(value), Some(old)) if value == old => {}
                    (Some(value), _) => {
                        pmc.set_raw(key.as_bytes(), value);
                    }
                    (None, Some(_)) => {
                        pmc.erase(key);
                    }
                    (None, None) => {}
                }
            }
        }
        self.file_shared.lock().dlds_saved = current;
    }

    /// Drain the managers' queued update broadcasts (the C++ sends them
    /// straight to `Td::send_update` from the managers).
    pub(crate) fn flush_user_updates(&mut self) {
        self.drive_files();
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
        let language_pack_updates = std::mem::take(&mut self.language_pack_updates);
        for update in language_pack_updates {
            self.send_update(update.into());
        }
        let animations_updates = std::mem::take(&mut self.animations_updates);
        for update in animations_updates {
            self.send_update(update.into());
        }
    }

    /// `AnimationsManager::AnimationsManager`
    /// (AnimationsManager.cpp:127-147) plus the `start_up` halves — the
    /// collapsed actor start reads the options and the `ans` pmc record
    /// (M.20).
    fn init_animations_manager(&mut self) {
        let mut env = TdAnimationsEnv {
            global: &mut self.global,
            options: self.option_manager.as_mut(),
            auth_is_bot: self.auth_manager.is_bot(),
            file_manager: Arc::clone(&self.file_manager),
            file_reference_manager: &mut self.file_reference_manager,
            updates: &mut self.animations_updates,
        };
        self.animations_manager.init(&mut env);
        let inline_updates = std::mem::take(&mut self.inline_updates);
        for update in inline_updates {
            self.send_update(update.into());
        }
        let message_query_updates = std::mem::take(&mut self.message_query_updates);
        for update in message_query_updates {
            self.send_update(update.into());
        }
        let poll_updates = std::mem::take(&mut self.poll_updates);
        for update in poll_updates {
            self.send_update(update.into());
        }
        let account_updates = std::mem::take(&mut self.account_updates);
        for update in account_updates {
            self.send_update(update.into());
        }
        let theme_updates = std::mem::take(&mut self.theme_updates);
        for update in theme_updates {
            self.send_update(update.into());
        }
    }

    /// `ThemeManager::init` (ThemeManager.cpp:439-452; the
    /// AuthManager.cpp:1552 call) — idempotent; the manager no-ops while
    /// unauthorized.
    fn init_theme_manager(&mut self) {
        let mut env = ThemeEnv {
            global: &mut self.global,
            options: self.option_manager.as_mut(),
            auth_is_authorized: self.auth_manager.is_authorized(),
            auth_is_bot: self.auth_manager.is_bot(),
            user_manager: &mut self.user_manager,
            chat_manager: &mut self.chat_manager,
            file_manager: std::sync::Arc::clone(&self.file_manager),
            updates: &mut self.theme_updates,
        };
        self.theme_manager.init(&mut env);
        let attach_menu_updates = std::mem::take(&mut self.attach_menu_updates);
        for update in attach_menu_updates {
            self.send_update(update.into());
        }
        let autosave_updates = std::mem::take(&mut self.autosave_updates);
        for update in autosave_updates {
            self.send_update(update.into());
        }
        let statistics_updates = std::mem::take(&mut self.statistics_updates);
        for update in statistics_updates {
            self.send_update(update.into());
        }
        let community_updates = std::mem::take(&mut self.community_updates);
        for update in community_updates {
            self.send_update(update.into());
        }
        let web_browser_updates = std::mem::take(&mut self.web_browser_updates);
        for update in web_browser_updates {
            self.send_update(update.into());
        }
        let call_updates = std::mem::take(&mut self.call_updates);
        for update in call_updates {
            self.send_update(update.into());
        }
        let transcription_updates = std::mem::take(&mut self.transcription_updates);
        for update in transcription_updates {
            self.send_update(update.into());
        }
        let translation_updates = std::mem::take(&mut self.translation_updates);
        for update in translation_updates {
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

    /// `LanguagePackManager::LanguagePackManager` (LanguagePackManager.cpp:200-226)
    /// plus `start_up` (cpp:228-250) — the collapsed actor start reads the
    /// restored options and opens the language pack database (M.24).
    fn init_language_pack_manager(&mut self) {
        let mut env = LanguagePackEnv {
            options: self.option_manager.as_mut(),
            updates: &mut self.language_pack_updates,
        };
        self.language_pack_manager.init(&mut env);
    }

    /// `TermsOfServiceManager::init` (AuthManager.cpp:1549) — the
    /// auth-success moment shared with `init_notification_manager`.
    fn init_terms_of_service_manager(&mut self) {
        let mut env = AccountEnv {
            global: &mut self.global,
            options: self.option_manager.as_mut(),
            user_manager: &mut self.user_manager,
            device_token_manager: &mut self.device_token_manager,
            auth_is_bot: self.auth_manager.is_bot(),
            updates: &mut self.account_updates,
        };
        self.terms_of_service_manager.init(&mut env);
    }

    /// `AttachMenuManager::init` (AttachMenuManager.cpp:366-430, also
    /// `start_up`; the C++ `AuthManager` on_get_authorization call at
    /// AuthManager.cpp:1542) — idempotent; the manager no-ops while inactive.
    fn init_attach_menu_manager(&mut self) {
        let mut env = AttachMenuEnv {
            global: &mut self.global,
            options: self.option_manager.as_mut(),
            auth_is_authorized: self.auth_manager.is_authorized(),
            auth_is_bot: self.auth_manager.is_bot(),
            user_manager: &mut self.user_manager,
            file_manager: std::sync::Arc::clone(&self.file_manager),
            file_reference_manager: &mut self.file_reference_manager,
            updates: &mut self.attach_menu_updates,
        };
        self.attach_menu_manager.init(&mut env);
    }

    /// The `StoryManager` constructor + `start_up` collapse
    /// (StoryManager.cpp:1999-2065, AuthManager.cpp:1545) — idempotent; the
    /// manager no-ops while disabled.
    fn init_story_manager(&mut self) {
        let crate::td::Td {
            story_manager,

            global,

            option_manager,

            auth_manager,

            user_manager,

            chat_manager,

            file_manager,

            notification_updates,
            ..
        } = self;

        let mut env = StoryEnv {
            global,

            options: option_manager.as_mut(),

            auth_is_authorized: auth_manager.is_authorized(),

            auth_is_bot: auth_manager.is_bot(),

            user_manager,

            chat_manager,

            file_manager: file_manager.clone(),

            updates: notification_updates,
        };

        story_manager.init(&mut env);
    }

    /// `make_unique<WebBrowserManager>` + its `start_up`
    /// (WebBrowserManager.cpp:142-144) — loads the saved settings only
    /// while authorized and non-bot; idempotent.
    fn init_web_browser_manager(&mut self) {
        let mut env = WebBrowserEnv {
            global: &mut self.global,
            auth_is_authorized: self.auth_manager.is_authorized(),
            auth_is_bot: self.auth_manager.is_bot(),
            updates: &mut self.web_browser_updates,
        };
        self.web_browser_manager.load_web_browser_settings(&mut env);
    }

    /// `WebBrowserManager::on_authorization_success`
    /// (WebBrowserManager.cpp:150-156, called from AuthManager.cpp:1556) —
    /// the auth-success moment shared with `init_notification_manager`.
    fn on_web_browser_authorization_success(&mut self) {
        let mut env = WebBrowserEnv {
            global: &mut self.global,
            auth_is_authorized: self.auth_manager.is_authorized(),
            auth_is_bot: self.auth_manager.is_bot(),
            updates: &mut self.web_browser_updates,
        };
        self.web_browser_manager.on_authorization_success(&mut env);
    }

    /// `TranslationManager::on_authorization_success`
    /// (TranslationManager.cpp:598-603, called from AuthManager.cpp:1554)
    /// — the live auth-success moment; runs before the web-browser
    /// manager's, in the C++ AuthData order. The binlog-restore paths
    /// don't call this (the C++ fires it only from on_get_authorization;
    /// the constructor restore already sent the styles update).
    fn on_translation_authorization_success(&mut self) {
        let mut env = TranslationEnv {
            global: &mut self.global,
            options: self.option_manager.as_mut(),
            auth_is_authorized: self.auth_manager.is_authorized(),
            auth_is_bot: self.auth_manager.is_bot(),
            updates: &mut self.translation_updates,
            users: &mut self.user_manager,
        };
        self.translation_manager.on_authorization_success(&mut env);
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
            let mut guard = self.deferred_results.lock();
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
                outbox.lock().push((id, result));
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
        // and web browser managers are the ones on this tree
        self.init_notification_manager();
        self.on_translation_authorization_success();
        self.on_web_browser_authorization_success();
        self.init_theme_manager();
        self.init_story_manager();
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
        for (message, pts) in &sink.edits {
            self.apply_edit_message(message, *pts);
        }
        for messages in &sink.deletions {
            self.apply_delete_messages(messages);
        }
        for (peer, max_id, top_msg_id) in &sink.reads_inbox {
            self.apply_read_history_inbox(peer, *max_id, *top_msg_id);
        }
        for (peer, max_id) in &sink.reads_outbox {
            self.apply_read_history_outbox(peer, *max_id);
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
        // The complete receive head (MessagesManager.cpp:11921-12107) over
        // the M.10.2c manager: parse → create → add_message_to_dialog with
        // the ordered insert, the counter maintenance and the
        // updateNewMessage emission inside. The five views alias one Td —
        // the sequential-call discipline of `split_context_alias` (no
        // take/restore: the ctx reads must see the real manager/global).
        // SAFETY: every alias below derives from `td_ptr` and the callee
        // uses them strictly one at a time.
        let td_ptr = std::ptr::from_mut(self);
        let ctx: &mut dyn crate::messages_manager::MessageObjectContext = unsafe { &mut *td_ptr };
        let message_info = crate::messages_manager::parse_telegram_api_message(
            ctx,
            message.clone(),
            false,
            false,
            source,
        );
        // `add_new_dialog` emits `updateNewChat` for the fresh dialog
        // (MessagesManager.cpp:33079+); the walking skeleton maps user
        // dialogs to its chat objects, the other dialog kinds ride the
        // ChatManager chat-object wiring.
        if message_info.dialog_id.get_type() == crate::messages_manager::DialogType::User {
            self.ensure_chat(message_info.dialog_id.get_user_id().get());
        }
        let manager: &mut crate::messages_manager::MessagesManager =
            unsafe { &mut (*td_ptr).messages_manager };
        let global: &mut crate::global::Global = unsafe { &mut (*td_ptr).global };
        let state: &mut dyn crate::messages_manager::ReadStateContext =
            unsafe { crate::td::split_context_alias(td_ptr) };
        let ctx: &mut dyn crate::messages_manager::MessageObjectContext = unsafe { &mut *td_ptr };
        let send_ctx: &dyn crate::messages_manager::SendMessageContext = unsafe { &*td_ptr };
        crate::messages_manager::on_get_message_full(
            global,
            manager,
            ctx,
            send_ctx,
            state,
            message_info,
            true,
            "updateNewMessage",
        );
    }

    /// The receive path for `updateEditMessage` (MessagesManager.cpp:5860):
    /// `had_message` in memory, the complete receive head with
    /// `from_update = false`, then the M.10.5 edit intake.
    fn apply_edit_message(&mut self, message: &telegram_api::Message, pts: i32) {
        let td_ptr = std::ptr::from_mut(self);
        // SAFETY: as apply_new_message.
        let ctx: &mut dyn crate::messages_manager::MessageObjectContext = unsafe { &mut *td_ptr };
        let message_info = crate::messages_manager::parse_telegram_api_message(
            ctx,
            message.clone(),
            false,
            false,
            "updateEditMessage",
        );
        let dialog_id = message_info.dialog_id;
        let message_id = message_info.message_id;
        // `have_message_force`'s database leg is the message-db seam — the
        // in-memory probe answers
        let had_message = unsafe { &*td_ptr }
            .messages_manager
            .get_dialog(dialog_id)
            .is_some_and(|d| crate::messages_manager::have_message_in_dialog_memory(d, message_id));
        let manager: &mut crate::messages_manager::MessagesManager =
            unsafe { &mut (*td_ptr).messages_manager };
        let global: &mut crate::global::Global = unsafe { &mut (*td_ptr).global };
        let state: &mut dyn crate::messages_manager::ReadStateContext =
            unsafe { crate::td::split_context_alias(td_ptr) };
        let ctx: &mut dyn crate::messages_manager::MessageObjectContext = unsafe { &mut *td_ptr };
        let send_ctx: &dyn crate::messages_manager::SendMessageContext = unsafe { &*td_ptr };
        let full_id = crate::messages_manager::on_get_message_full(
            global,
            manager,
            ctx,
            send_ctx,
            state,
            message_info,
            false,
            "updateEditMessage",
        );
        if full_id.get_dialog_id() != crate::messages_manager::DialogId::default() {
            let is_bot = self.auth_manager.is_bot();
            let mut m = self
                .messages_manager
                .get_dialog_mut(dialog_id)
                .and_then(|d| d.messages.remove(&message_id));
            if let Some(m) = m.as_mut() {
                let send_edited = crate::messages_manager::on_message_edited(
                    &mut self.messages_manager,
                    dialog_id,
                    m,
                    pts,
                    had_message,
                    is_bot,
                );
                if send_edited {
                    // `send_update_message_edited` (MessagesManager.cpp:
                    // 5985-6014's emission half)
                    let edit_date = crate::messages_manager::get_message_shown_edit_date(m);
                    let reply_markup = m.reply_markup.as_ref().and_then(|reply_markup| {
                        crate::messages_manager::get_message_reply_markup_object(self, reply_markup)
                    });
                    self.send_update(Object::updateMessageEdited(td_api::updateMessageEdited {
                        chat_id: dialog_id.get(),
                        message_id: message_id.get(),
                        edit_date,
                        reply_markup,
                    }));
                }
            }
            if let Some(m) = m {
                self.messages_manager
                    .get_dialog_mut(dialog_id)
                    .and_then(|d| d.messages.insert(message_id, m));
            }
        }
    }

    /// The receive path for `updateReadHistoryInbox`
    /// (MessagesManager.cpp:5885-5901): the folder read is the M.6 dialog
    /// seam and the forum `top_msg_id` arm the topic-manager seam — the
    /// plain arm runs the M.10.7 intake.
    fn apply_read_history_inbox(
        &mut self,
        peer: &telegram_api::Peer,
        max_id: i32,
        top_msg_id: i32,
    ) {
        if top_msg_id != 0 {
            // `on_update_forum_topic_unread` — the ForumTopicManager seam
            return;
        }
        let dialog_id = crate::messages_manager::DialogId::from_peer(peer);
        if !dialog_id.is_valid() {
            return;
        }
        let last_read_inbox_message_id = crate::messages_manager::MessageId::from_server_message_id(
            crate::messages_manager::ServerMessageId::new(max_id),
        );
        // SAFETY: the alias pair derives from `td_ptr`; the callee uses
        // them strictly one at a time.
        let td_ptr = std::ptr::from_mut(self);
        let state: &mut dyn crate::messages_manager::ReadStateContext =
            unsafe { crate::td::split_context_alias(td_ptr) };
        let manager: &mut crate::messages_manager::MessagesManager =
            unsafe { &mut (*td_ptr).messages_manager };
        let ctx: &dyn crate::messages_manager::MessageObjectContext = unsafe { &*td_ptr };
        crate::messages_manager::read_history_inbox(
            manager,
            ctx,
            state,
            dialog_id,
            last_read_inbox_message_id,
            -1,
            "updateReadHistoryInbox",
        );
    }

    /// The receive path for `updateReadHistoryOutbox`
    /// (MessagesManager.cpp:5903-5910).
    fn apply_read_history_outbox(&mut self, peer: &telegram_api::Peer, max_id: i32) {
        let dialog_id = crate::messages_manager::DialogId::from_peer(peer);
        if !dialog_id.is_valid() {
            return;
        }
        let max_message_id = crate::messages_manager::MessageId::from_server_message_id(
            crate::messages_manager::ServerMessageId::new(max_id),
        );
        // SAFETY: as apply_read_history_inbox.
        let td_ptr = std::ptr::from_mut(self);
        let state: &mut dyn crate::messages_manager::ReadStateContext =
            unsafe { crate::td::split_context_alias(td_ptr) };
        let manager: &mut crate::messages_manager::MessagesManager =
            unsafe { &mut (*td_ptr).messages_manager };
        let ctx: &dyn crate::messages_manager::MessageObjectContext = unsafe { &*td_ptr };
        crate::messages_manager::read_history_outbox(
            manager,
            ctx,
            state,
            dialog_id,
            max_message_id,
            0,
        );
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

// ═══ M.12.10 T.1 — the `Td *td_`/`G()` plane of the files managers ═══
//
// The C++ file contexts hold a raw `Td *` and read the option/time/db
// surfaces at call time; the collapsed managers own their contexts in
// `'static` boxes, so the shared plane lives in [`TdFileShared`] behind
// `Arc<parking_lot::Mutex<>>` (the `ResourceManager` handle convention). Td refreshes
// the snapshot when a drive starts ([`Td::drive_files`]) and drains the
// outboxes after each loader pass — the collapsed `send_closure` hops.

/// The [`FileManagerWorker`] effects that must re-enter `FileManager`
/// (they fire while `&mut FileManager` is held, so the Td replays them
/// through its `Rc` handle after the driving call returns).
pub(crate) enum FileWorkerEvent {
    CancelDownload(Box<FileNode>),
    CancelUpload(Box<FileNode>),
    CancelGenerate(Box<FileNode>),
    FinishDownload(FileId),
    FinishUpload(FileId),
    RunNode(Box<FileNode>),
    /// `Context::on_merge_files` → `FileReferenceManager::merge`.
    MergeFiles(FileId, FileId),
}

/// The [`FileManagerWorker`] file-db effects, replayed against TdDb's
/// "files" kv (the C++ calls `G()->td_db()->get_file_db()` inline).
pub(crate) enum FileDbEvent {
    /// `flush_node_info`/`flush_node_full` — write the node record.
    Flush {
        node: Box<FileNode>,
        other_pmc_id: FileDbId,
        new_remote: bool,
        new_local: bool,
        new_generate: bool,
    },
    /// `clear_from_pmc` — delete the node record.
    Clear { node: Box<FileNode> },
}

/// The per-drive [`TdFileShared`] snapshot — the C++ contexts read the
/// same surfaces off `Td*`/`G()` at call time; the cell takes them in one
/// refresh per drive.
pub(crate) struct TdFileSnapshot {
    files_dir: String,
    secure_files_dir: String,
    is_bot: bool,
    use_message_database: bool,
    use_file_database: bool,
    use_storage_optimizer: bool,
    ignore_inline_thumbnails: bool,
    option_integers: BTreeMap<String, i64>,
    unix_time: i32,
    /// `TdDb::with_db_path` — the binlog/sqlite paths (the fast-stat
    /// database size and the FileManager `bad_paths_`).
    database_paths: Vec<String>,
    /// `G()->is_test_dc()` — the `webfile_dc_id` default source.
    is_test_dc: bool,
    /// `G()->get_option_boolean("ignore_file_names")` — the
    /// `for_suggested_file_name` first arm.
    ignore_file_names: bool,
}

/// The shared files plane: the manager handles the stored boxes reach
/// through, the update/action outboxes, and the refreshed option/time
/// snapshot.
pub(crate) struct TdFileShared {
    file_manager: Arc<parking_lot::Mutex<FileManager>>,
    file_load_manager: Arc<parking_lot::Mutex<FileLoadManager>>,
    /// `Td::send_update` outbox (updateFileDownloads,
    /// updateFileAddedToDownloads, updateFileGeneration*, ...).
    pub(crate) updates: Vec<td_api::Update>,
    /// `StorageManager::on_new_file` hop.
    pub(crate) new_file_events: Vec<(i64, i64, i32)>,
    /// The worker replay queue.
    pub(crate) worker_events: Vec<FileWorkerEvent>,
    /// The file-db effects queue.
    pub(crate) file_db_events: Vec<FileDbEvent>,
    /// The `make_download_file_callback` state hops — deferred because
    /// the FileManager borrow is held while the loader runs.
    pub(crate) download_state_events: Arc<parking_lot::Mutex<Vec<(FileId, bool)>>>,
    /// The `UserDownloadFileCallback` hops (FileManager.cpp:1268-1283) —
    /// both completions re-enter `on_user_file_download_finished` at the
    /// drive end.
    pub(crate) user_download_finished_events: Arc<parking_lot::Mutex<Vec<FileId>>>,
    // ── the snapshot (refreshed per drive) ──
    files_dir: String,
    secure_files_dir: String,
    is_bot: bool,
    use_message_database: bool,
    use_file_database: bool,
    use_storage_optimizer: bool,
    ignore_inline_thumbnails: bool,
    option_integers: BTreeMap<String, i64>,
    unix_time: i32,
    /// `TdDb::with_db_path` — the binlog/sqlite paths (the fast-stat
    /// database size and the FileManager `bad_paths_`).
    database_paths: Vec<String>,
    /// `G()->is_test_dc()` — the `webfile_dc_id` default source.
    is_test_dc: bool,
    /// `G()->get_option_boolean("ignore_file_names")` — the
    /// `for_suggested_file_name` first arm.
    ignore_file_names: bool,
    /// The `tmp_file_id`/`perm_file_id` pmc counters of
    /// `open_temp_file`/`create_from_temp` (the increments stay in the
    /// snapshot for the run — the random-suffix fallbacks cover a
    /// restart's reused ids).
    file_name_counters: crate::files::FileNameCounters,
    /// The DownloadManager `dlds#` keyspace shadow — the stored context
    /// box cannot borrow the binlog pmc, so the keyspace lives here:
    /// preloaded from the real pmc at init and flushed back after drives
    /// once the record layout is unified (T.7, the D1 gate).
    pub(crate) dlds: BTreeMap<String, Vec<u8>>,
    pub(crate) dlds_saved: BTreeMap<String, Vec<u8>>,
    pub(crate) dlds_pmc_live: bool,
}

impl TdFileShared {
    fn new(
        file_manager: Arc<parking_lot::Mutex<FileManager>>,
        file_load_manager: Arc<parking_lot::Mutex<FileLoadManager>>,
    ) -> TdFileShared {
        TdFileShared {
            file_manager,
            file_load_manager,
            updates: Vec::new(),
            new_file_events: Vec::new(),
            worker_events: Vec::new(),
            file_db_events: Vec::new(),
            download_state_events: Arc::new(Mutex::new(Vec::new())),
            user_download_finished_events: Arc::new(Mutex::new(Vec::new())),
            files_dir: String::new(),
            secure_files_dir: String::new(),
            is_bot: false,
            use_message_database: false,
            use_file_database: false,
            use_storage_optimizer: false,
            ignore_inline_thumbnails: false,
            option_integers: BTreeMap::new(),
            unix_time: 0,
            database_paths: Vec::new(),
            is_test_dc: false,
            ignore_file_names: false,
            file_name_counters: crate::files::FileNameCounters::default(),
            dlds: BTreeMap::new(),
            dlds_saved: BTreeMap::new(),
            dlds_pmc_live: false,
        }
    }

    pub(crate) fn refresh(&mut self, snapshot: TdFileSnapshot) {
        let TdFileSnapshot {
            files_dir,
            secure_files_dir,
            is_bot,
            use_message_database,
            use_file_database,
            use_storage_optimizer,
            ignore_inline_thumbnails,
            option_integers,
            unix_time,
            database_paths,
            is_test_dc,
            ignore_file_names,
        } = snapshot;
        self.files_dir = files_dir;
        self.secure_files_dir = secure_files_dir;
        self.is_bot = is_bot;
        self.use_message_database = use_message_database;
        self.use_file_database = use_file_database;
        self.use_storage_optimizer = use_storage_optimizer;
        self.ignore_inline_thumbnails = ignore_inline_thumbnails;
        self.option_integers = option_integers;
        self.unix_time = unix_time;
        self.database_paths = database_paths;
        self.is_test_dc = is_test_dc;
        self.ignore_file_names = ignore_file_names;
    }

    /// The `TdDb::with_db_path` snapshot read.
    pub(crate) fn database_paths(&self) -> Vec<String> {
        self.database_paths.clone()
    }

    /// The dlds# keyspace preload (the init-time pmc prefix read).
    pub(crate) fn dlds_preload(&mut self, records: Vec<(String, Vec<u8>)>) {
        self.dlds = records.into_iter().collect();
        self.dlds_saved = self.dlds.clone();
    }

    // ── the snapshot reads the storage/files planes share ──

    pub(crate) fn files_dir(&self) -> String {
        self.files_dir.clone()
    }

    pub(crate) fn secure_files_dir(&self) -> String {
        self.secure_files_dir.clone()
    }

    pub(crate) fn option_integer(&self, name: &str, default: i64) -> i64 {
        self.option_integers.get(name).copied().unwrap_or(default)
    }

    pub(crate) fn use_file_database(&self) -> bool {
        self.use_file_database
    }

    pub(crate) fn use_storage_optimizer(&self) -> bool {
        self.use_storage_optimizer
    }

    pub(crate) fn is_test_dc(&self) -> bool {
        self.is_test_dc
    }

    /// `open_temp_file` (FileLoaderUtils.cpp:85-102) over the snapshot.
    pub(crate) fn open_temp_file(
        &mut self,
        file_type: FileType,
    ) -> Result<(std::fs::File, String), Status> {
        crate::files::open_temp_file_impl(
            file_type,
            &self.files_dir,
            &self.secure_files_dir,
            &mut self.file_name_counters,
        )
    }

    /// `create_from_temp` (FileLoaderUtils.cpp:126-140) over the snapshot.
    pub(crate) fn create_from_temp(
        &mut self,
        file_type: FileType,
        path: &str,
        name: &str,
    ) -> Result<String, Status> {
        crate::files::create_from_temp_impl(
            file_type,
            path,
            name,
            &self.files_dir,
            &self.secure_files_dir,
            self.ignore_file_names,
            &mut self.file_name_counters,
        )
    }

    /// The `tmp_file_id`/`perm_file_id` pmc preload (the init-time read).
    pub(crate) fn preload_file_name_counters(&mut self, tmp_file_id: i32, perm_file_id: i32) {
        self.file_name_counters = crate::files::FileNameCounters {
            tmp_file_id,
            perm_file_id,
        };
    }
}

// ── the `'static` boxes the managers store — thin delegates to the cell ──

/// The stored `FileManager::Context`/`DownloadManager::Callback`/
/// `DownloadManager::Context` planes (the C++ `FileManagerContext(td)` /
/// `DownloadManagerCallback(td, ...)` objects).
pub(crate) struct TdFilePlane(pub(crate) Arc<parking_lot::Mutex<TdFileShared>>);

impl FileManagerContext for TdFilePlane {
    fn keep_exact_remote_location(&self) -> bool {
        !self.0.lock().is_bot
    }

    fn files_dir(&self) -> String {
        self.0.lock().files_dir.clone()
    }

    fn secure_files_dir(&self) -> String {
        self.0.lock().secure_files_dir.clone()
    }

    fn option_integer(&self, name: &str, default: i64) -> i64 {
        self.0
            .lock()
            .option_integers
            .get(name)
            .copied()
            .unwrap_or(default)
    }

    fn need_notify_on_new_files(&self) -> bool {
        !self.0.lock().is_bot
    }

    fn on_new_file(&mut self, size: i64, real_size: i64, cnt: i32) {
        self.0.lock().new_file_events.push((size, real_size, cnt));
    }

    /// `FileReferenceManager::repair_file_reference` — the repair queries
    /// are [blocked: net]; the collapse answers the failure the C++
    /// promise would eventually carry.
    fn repair_file_reference(&mut self, _file_id: FileId) -> Result<(), tdutils::status::Status> {
        Err(tdutils::status::Status::error_message(
            "Failed to repair file reference",
        ))
    }

    /// `FileReferenceManager::reload_photo` — [blocked: net] like the
    /// repair seam above.
    fn reload_photo(&mut self, _file_id: FileId) -> Result<(), tdutils::status::Status> {
        Err(tdutils::status::Status::error_message(
            "Failed to reload photo",
        ))
    }

    fn ignore_inline_thumbnails(&self) -> bool {
        self.0.lock().ignore_inline_thumbnails
    }

    fn on_file_generation_start(
        &mut self,
        generation_id: i64,
        original_path: &str,
        path: &str,
        conversion: &str,
    ) {
        self.0
            .lock()
            .updates
            .push(td_api::Update::updateFileGenerationStart(
                td_api::updateFileGenerationStart {
                    generation_id,
                    original_path: original_path.to_owned(),
                    destination_path: path.to_owned(),
                    conversion: conversion.to_owned(),
                },
            ));
    }

    fn on_file_generation_stop(&mut self, generation_id: i64) {
        self.0
            .lock()
            .updates
            .push(td_api::Update::updateFileGenerationStop(
                td_api::updateFileGenerationStop { generation_id },
            ));
    }
}

impl DownloadManagerContext for TdFilePlane {
    fn use_message_database(&self) -> bool {
        self.0.lock().use_message_database
    }

    fn pmc_get(&mut self, key: &str) -> Vec<u8> {
        self.0.lock().dlds.get(key).cloned().unwrap_or_default()
    }

    fn pmc_set(&mut self, key: &str, value: &[u8]) {
        self.0.lock().dlds.insert(key.to_owned(), value.to_vec());
    }

    fn pmc_erase(&mut self, key: &str) {
        self.0.lock().dlds.remove(key);
    }

    fn pmc_erase_by_prefix(&mut self, prefix: &str) {
        self.0.lock().dlds.retain(|key, _| !key.starts_with(prefix));
    }

    fn pmc_prefix_get(&mut self, prefix: &str) -> Vec<(String, Vec<u8>)> {
        self.0
            .lock()
            .dlds
            .iter()
            .filter(|(key, _)| key.starts_with(prefix))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect()
    }

    fn unix_time(&mut self) -> i32 {
        self.0.lock().unix_time
    }

    fn store_file_id(&mut self, file_id: FileId, out: &mut Vec<u8>) {
        // the versioned `FileManager::store_file` serialization (T.7, the
        // D1 unification — the dlds records are byte-compatible with the
        // C++ DownloadManager pmc layout)
        let file_manager = Arc::clone(&self.0.lock().file_manager);
        file_manager.lock().store_file(file_id, out);
    }

    fn parse_file_id(
        &mut self,
        parser: &mut crate::store_parse::Parser,
    ) -> Result<FileId, tdutils::status::Status> {
        let file_manager = Arc::clone(&self.0.lock().file_manager);
        file_manager
            .lock()
            .parse_file(parser)
            .map_err(tdutils::status::Status::error_message)
    }
}

impl DownloadManagerCallback for TdFilePlane {
    fn update_counters(&mut self, counters: DownloadCounters) {
        let mut shared = self.0.lock();
        if !shared.is_bot {
            shared.updates.push(td_api::Update::updateFileDownloads(
                counters.get_update_file_downloads_object(),
            ));
        }
    }

    fn update_file_added(
        &mut self,
        file_id: FileId,
        file_source_id: FileSourceId,
        add_date: i32,
        complete_date: i32,
        is_paused: bool,
        counters: crate::files::DownloadFileCounters,
    ) {
        let file_download = self.get_file_download_object(
            file_id,
            file_source_id,
            add_date,
            complete_date,
            is_paused,
        );
        let mut shared = self.0.lock();
        shared
            .updates
            .push(td_api::Update::updateFileAddedToDownloads(
                td_api::updateFileAddedToDownloads {
                    file_download,
                    counts: Some(counters.get_downloaded_file_counts_object()),
                },
            ));
    }

    fn update_file_changed(
        &mut self,
        file_id: FileId,
        complete_date: i32,
        is_paused: bool,
        counters: crate::files::DownloadFileCounters,
    ) {
        self.0
            .lock()
            .updates
            .push(td_api::Update::updateFileDownload(
                td_api::updateFileDownload {
                    file_id: file_id.get(),
                    complete_date,
                    is_paused,
                    counts: Some(counters.get_downloaded_file_counts_object()),
                },
            ));
    }

    fn update_file_removed(
        &mut self,
        file_id: FileId,
        counters: crate::files::DownloadFileCounters,
    ) {
        self.0
            .lock()
            .updates
            .push(td_api::Update::updateFileRemovedFromDownloads(
                td_api::updateFileRemovedFromDownloads {
                    file_id: file_id.get(),
                    counts: Some(counters.get_downloaded_file_counts_object()),
                },
            ));
    }

    /// `FileManager::get_internal_download_id` (the shared id space the
    /// download queries ride).
    fn get_internal_download_id(&mut self) -> i64 {
        let file_manager = Arc::clone(&self.0.lock().file_manager);
        file_manager.lock().get_internal_download_id()
    }

    /// `DownloadManagerCallback::start_file` (DownloadManagerCallback.cpp:60)
    /// — `FileManager::download` with the bridge callback, offset/limit -1.
    /// The handles clone out of the shared cell first: the lock order is
    /// always manager → shared (the worker callbacks re-enter the cell
    /// under the manager borrow), never shared → manager.
    fn start_file(&mut self, file_id: FileId, internal_download_id: i64, priority: i8) {
        let (file_manager, file_load_manager, events) = {
            let shared = self.0.lock();
            (
                Arc::clone(&shared.file_manager),
                Arc::clone(&shared.file_load_manager),
                Arc::clone(&shared.download_state_events),
            )
        };
        let callback: Arc<parking_lot::Mutex<dyn DownloadCallback>> =
            Arc::new(Mutex::new(TdDownloadBridge { events }));
        file_manager.lock().download(
            file_id,
            internal_download_id,
            callback,
            i32::from(priority),
            -1,
            -1,
            &mut file_load_manager.lock(),
        );
    }

    /// `DownloadManagerCallback::pause_file` — `FileManager::cancel_download`
    /// with `remove_from_download_manager = false`
    /// (DownloadManagerCallback.cpp:66-68).
    fn pause_file(&mut self, file_id: FileId, internal_download_id: i64) {
        let (file_manager, file_load_manager) = {
            let shared = self.0.lock();
            (
                Arc::clone(&shared.file_manager),
                Arc::clone(&shared.file_load_manager),
            )
        };
        file_manager.lock().cancel_download(
            file_id,
            internal_download_id,
            false,
            &mut file_load_manager.lock(),
        );
    }

    /// `DownloadManagerCallback::delete_file` — routed through the gc
    /// unlink plane (the local copy drops, the pmc record clears, the
    /// download manager reaps the finished query); the C++
    /// `FileManager::delete_file` promise surface has no td_api
    /// observable here.
    fn delete_file(&mut self, file_id: FileId) {
        let file_manager = Arc::clone(&self.0.lock().file_manager);
        let location = file_manager
            .lock()
            .get_file_view(file_id)
            .and_then(|view| view.get_full_local_location().cloned());
        if let Some(location) = location {
            file_manager.lock().on_file_unlink(&location);
        }
    }

    /// `get_file_search_text` — the C++ asks
    /// `FileReferenceManager::get_file_search_text` (the message-plane
    /// search text, M.10's seam); the empty text keeps the download
    /// listed like a dialog-less file.
    fn get_file_search_text(
        &mut self,
        _file_id: FileId,
        _file_source_id: FileSourceId,
    ) -> Result<String, tdutils::status::Status> {
        Ok(String::new())
    }

    /// `get_sync_file_view` collapsed to the three sizes
    /// (`add_file_info`'s size/expected_size/downloaded_size reads,
    /// DownloadManager.cpp:532-535); the `check_local_location` half
    /// re-stats through the drive.
    fn sync_file_sizes(&mut self, file_id: FileId) -> (i64, i64, i64) {
        let file_manager = Arc::clone(&self.0.lock().file_manager);
        file_manager
            .lock()
            .get_file_view(file_id)
            .map(|view: FileView<'_>| {
                (
                    view.size(),
                    view.expected_size(false),
                    view.local_total_size(),
                )
            })
            .unwrap_or((0, 0, 0))
    }

    fn get_file_object(&mut self, file_id: FileId) -> Option<td_api::File> {
        let file_manager = Arc::clone(&self.0.lock().file_manager);
        Some(file_manager.lock().get_file_object(file_id))
    }

    fn get_file_download_object(
        &mut self,
        file_id: FileId,
        _file_source_id: FileSourceId,
        add_date: i32,
        complete_date: i32,
        is_paused: bool,
    ) -> Option<td_api::FileDownload> {
        let file_manager = Arc::clone(&self.0.lock().file_manager);
        let file_id = file_manager
            .lock()
            .get_file_view(file_id)
            .map(|view| view.get_main_file_id().get())
            .unwrap_or(file_id.get());
        // the message plane (the source-message resolution) is the M.10
        // seam; the C++ `message_ == nullptr` filter applies downstream
        Some(td_api::FileDownload {
            file_id,
            message: None,
            add_date,
            complete_date,
            is_paused,
        })
    }
}

/// The `make_download_file_callback` bridge (DownloadManagerCallback.cpp:99-121)
/// — the loader-side completions hop into
/// `DownloadManager::update_file_download_state` at the drive end.
struct TdDownloadBridge {
    events: Arc<parking_lot::Mutex<Vec<(FileId, bool)>>>,
}

impl DownloadCallback for TdDownloadBridge {
    fn on_progress(&mut self, file_id: FileId) {
        self.events.lock().push((file_id, false));
    }

    fn on_download_ok(&mut self, file_id: FileId) {
        self.events.lock().push((file_id, false));
    }

    fn on_download_error(&mut self, file_id: FileId, _status: tdutils::status::Status) {
        self.events.lock().push((file_id, true));
    }
}

/// `FileManager::UserDownloadFileCallback` (FileManager.cpp:1268-1283) —
/// both completions of a user download release the parked synchronous
/// `downloadFile` promises at the drive end.
struct TdUserDownloadCallback {
    events: Arc<parking_lot::Mutex<Vec<FileId>>>,
}

impl DownloadCallback for TdUserDownloadCallback {
    fn on_download_ok(&mut self, file_id: FileId) {
        self.events.lock().push(file_id);
    }

    fn on_download_error(&mut self, file_id: FileId, _status: tdutils::status::Status) {
        self.events.lock().push(file_id);
    }
}

/// The stored `FileManager::Worker` plane — the fm-touching effects are
/// queued (the hooks fire under `&mut FileManager`); the DownloadManager
/// reap is a separate handle and runs inline like the C++ `send_closure`.
pub(crate) struct TdFileWorker {
    pub(crate) shared: Arc<parking_lot::Mutex<TdFileShared>>,
    pub(crate) download_manager: Arc<parking_lot::Mutex<DownloadManager>>,
}

impl FileManagerWorker for TdFileWorker {
    fn cancel_download(&mut self, node: &FileNode) {
        self.shared
            .lock()
            .worker_events
            .push(FileWorkerEvent::CancelDownload(Box::new(node.clone())));
    }

    fn cancel_upload(&mut self, node: &FileNode) {
        self.shared
            .lock()
            .worker_events
            .push(FileWorkerEvent::CancelUpload(Box::new(node.clone())));
    }

    fn cancel_generate(&mut self, node: &FileNode) {
        self.shared
            .lock()
            .worker_events
            .push(FileWorkerEvent::CancelGenerate(Box::new(node.clone())));
    }

    fn finish_download(&mut self, file_id: FileId) {
        self.shared
            .lock()
            .worker_events
            .push(FileWorkerEvent::FinishDownload(file_id));
    }

    fn finish_upload(&mut self, file_id: FileId) {
        self.shared
            .lock()
            .worker_events
            .push(FileWorkerEvent::FinishUpload(file_id));
    }

    fn clear_from_pmc(&mut self, node: &FileNode) {
        self.shared.lock().file_db_events.push(FileDbEvent::Clear {
            node: Box::new(node.clone()),
        });
    }

    fn remove_file_if_finished(&mut self, file_id: FileId) {
        let _ = self
            .download_manager
            .lock()
            .remove_file_if_finished(file_id);
    }

    fn run_node(&mut self, node: &FileNode) {
        self.shared
            .lock()
            .worker_events
            .push(FileWorkerEvent::RunNode(Box::new(node.clone())));
    }

    fn flush_node_info(&mut self, node: &FileNode) {
        self.shared.lock().file_db_events.push(FileDbEvent::Flush {
            node: Box::new(node.clone()),
            other_pmc_id: FileDbId::default(),
            new_remote: false,
            new_local: false,
            new_generate: false,
        });
    }

    fn flush_node_full(
        &mut self,
        node: &FileNode,
        new_remote: bool,
        new_local: bool,
        new_generate: bool,
        other_pmc_id: FileDbId,
    ) {
        self.shared.lock().file_db_events.push(FileDbEvent::Flush {
            node: Box::new(node.clone()),
            other_pmc_id,
            new_remote,
            new_local,
            new_generate,
        });
    }

    fn on_merge_files(&mut self, from_file_id: FileId, to_file_id: FileId) {
        self.shared
            .lock()
            .worker_events
            .push(FileWorkerEvent::MergeFiles(from_file_id, to_file_id));
    }
}

// ── the AnimationsManager environment (M.20) ──
//
// The C++ AnimationsManager reads `G()` (options, close flag, the binlog
// + sqlite pmcs, the FileManager) and sends its queries from the
// collapsed actor's `create_handler`s; the env carries the same surfaces
// and the `[blocked: net]` query seams.

/// The per-call [`crate::animations_manager::AnimationsManagerEnv`] over
/// the Td fields — built per call with the field-split borrows, never
/// held across calls.
pub(crate) struct TdAnimationsEnv<'a> {
    pub(crate) global: &'a mut Global,
    pub(crate) options: Option<&'a mut OptionManager>,
    pub(crate) auth_is_bot: bool,
    pub(crate) file_manager: std::sync::Arc<parking_lot::Mutex<crate::files::FileManager>>,
    pub(crate) file_reference_manager: &'a mut crate::files::FileReferenceManager,
    pub(crate) updates: &'a mut Vec<td_api::Update>,
}

impl crate::photo::DialogPhotoStoreEnv for TdAnimationsEnv<'_> {
    fn store_file_id(&mut self, file_id: FileId, out: &mut Vec<u8>) {
        self.file_manager.lock().store_file(file_id, out);
    }

    fn parse_file_id(
        &mut self,
        parser: &mut crate::store_parse::Parser<'_>,
    ) -> Result<FileId, &'static str> {
        self.file_manager.lock().parse_file(parser)
    }
}

impl crate::animations_manager::AnimationsManagerEnv for TdAnimationsEnv<'_> {
    fn auth_is_bot(&self) -> bool {
        self.auth_is_bot
    }

    fn close_flag(&self) -> bool {
        self.global.close_flag()
    }

    fn get_option_string(&self, name: &str, default_value: &str) -> String {
        self.options.as_ref().map_or_else(
            || default_value.to_owned(),
            |options| options.get_option_string(name, default_value),
        )
    }

    fn get_option_integer(&self, name: &str, default_value: i64) -> i64 {
        self.options.as_ref().map_or(default_value, |options| {
            options.get_option_integer(name, default_value)
        })
    }

    fn binlog_pmc_erase(&mut self, key: &str) {
        self.global.binlog_pmc().erase(key);
    }

    fn use_sqlite_pmc(&self) -> bool {
        self.global.use_sqlite_pmc()
    }

    fn sqlite_pmc_get_raw(&mut self, key: &[u8]) -> Vec<u8> {
        // The C++ sqlite pmc `get` is async; the collapsed line inlines
        // the callback (the common kv plays the pmc role on this line).
        self.global.td_db().common_kv().get(key)
    }

    fn sqlite_pmc_set_raw(&mut self, key: &[u8], value: &[u8]) {
        self.global.td_db().common_kv().set(key, value);
    }

    fn send_update(&mut self, update: td_api::Update) {
        self.updates.push(update);
    }

    fn file_manager(&self) -> std::sync::Arc<parking_lot::Mutex<crate::files::FileManager>> {
        std::sync::Arc::clone(&self.file_manager)
    }

    fn create_saved_animations_file_source(&mut self) -> crate::files::FileSourceId {
        self.file_reference_manager
            .intern(crate::files::FileSource::SavedAnimations)
    }

    fn change_files_source(
        &mut self,
        file_source_id: crate::files::FileSourceId,
        old_file_ids: &[FileId],
        new_file_ids: &[FileId],
        _source: &str,
    ) {
        // `FileManager::change_files_source` over the main-file-id sets.
        let file_manager = self.file_manager.lock();
        let main_file_ids = |file_manager: &crate::files::FileManager,
                             file_ids: &[FileId]|
         -> std::collections::HashSet<FileId> {
            file_ids
                .iter()
                .map(|file_id| {
                    file_manager
                        .get_file_view(*file_id)
                        .map(|view| view.get_main_file_id())
                        .unwrap_or_default()
                })
                .collect()
        };
        let old_main = main_file_ids(&file_manager, old_file_ids);
        let mut new_main = main_file_ids(&file_manager, new_file_ids);
        for file_id in old_main {
            if !new_main.remove(&file_id) {
                self.file_reference_manager
                    .remove_file_source(file_id, file_source_id);
            }
        }
        for file_id in new_main {
            self.file_reference_manager
                .add_file_source(file_id, file_source_id);
        }
    }

    fn on_get_document_animation(&mut self, _document: telegram_api::document) -> Option<FileId> {
        // `td_->documents_manager_->on_get_document` — the
        // [blocked: M.28] intake seam; answers None until the document
        // plane lands, which the caller handles like the C++
        // non-animation case.
        None
    }

    fn send_get_saved_gifs_query(&mut self, is_repair: bool, hash: i64) {
        tdutils::log_info!(
            "Send GetSavedGifsQuery(is_repair = {is_repair}, hash = {hash}) — [blocked: net]"
        );
    }

    fn send_save_gif_query_net(
        &mut self,
        file_id: FileId,
        unsave: bool,
        _input_document: telegram_api::InputDocument,
        _promise: Promise<()>,
    ) {
        tdutils::log_info!(
            "Send SaveGifQuery(file_id = {}, unsave = {unsave}) — [blocked: net]",
            file_id.get()
        );
    }
}

/// The `&self` option slice for the `LinkManager` statics
/// (`get_instant_view_link*`, LinkManager.h:115-119) — they read
/// `G()->get_option_string` only, so no manager instance is needed.
pub(crate) struct TdStaticLinkEnv<'a> {
    pub(crate) options: Option<&'a OptionManager>,
}

impl crate::link_manager::LinkEnv for TdStaticLinkEnv<'_> {
    fn get_option_string_with_default(&self, name: &str, default_value: &str) -> String {
        self.options.as_ref().map_or_else(
            || default_value.to_owned(),
            |options| options.get_option_string(name, default_value),
        )
    }

    fn get_option_string(&self, name: &str) -> String {
        self.options
            .as_ref()
            .map_or_else(String::new, |options| options.get_option_string(name, ""))
    }
}

/// The per-call [`crate::link_manager::LinkEnv`] over the Td fields —
/// built per arm with the field-split borrows, never held across calls.
pub(crate) struct TdLinkEnv<'a> {
    pub(crate) global: &'a mut Global,
    pub(crate) options: Option<&'a mut OptionManager>,
}

impl crate::link_manager::LinkEnv for TdLinkEnv<'_> {
    fn get_option_string_with_default(&self, name: &str, default_value: &str) -> String {
        self.options.as_ref().map_or_else(
            || default_value.to_owned(),
            |options| options.get_option_string(name, default_value),
        )
    }

    fn get_option_string(&self, name: &str) -> String {
        self.options
            .as_ref()
            .map_or_else(String::new, |options| options.get_option_string(name, ""))
    }

    /// The option half of `InternalLinkBotStart`'s autostart
    /// (LinkManager.cpp:638-640): the premium-bot username autostarts.
    fn bot_start_autostart(&self, bot_username: &str) -> bool {
        self.get_option_string("premium_bot_username") == bot_username
    }

    fn close_flag(&self) -> bool {
        self.global.close_flag()
    }

    fn binlog_pmc_get(&mut self, key: &str) -> String {
        self.global.binlog_pmc().get(key)
    }

    fn binlog_pmc_set(&mut self, key: &str, value: &str) {
        self.global.binlog_pmc().set(key, value);
    }
}

// ── the StorageManager environment (T.5/T.8) ──
//
// The C++ StorageManager/FileStatsWorker read `G()` (options, the binlog
// pmc, TdDb, the FileManager) inline; the collapsed env takes the same
// surfaces per drive — the shared files snapshot for the option reads,
// the Global for the pmc/TdDb planes.

/// The per-drive [`crate::files::StorageContext`] over the Td fields —
/// built per call with the field-split borrows, never held across calls.
pub(crate) struct TdStorageEnv<'a> {
    shared: Arc<parking_lot::Mutex<TdFileShared>>,
    global: &'a mut Global,
    language_pack_database_path: String,
}

impl FileManagerContext for TdStorageEnv<'_> {
    fn files_dir(&self) -> String {
        self.shared.lock().files_dir()
    }

    fn secure_files_dir(&self) -> String {
        self.shared.lock().secure_files_dir()
    }

    fn option_integer(&self, name: &str, default: i64) -> i64 {
        self.shared.lock().option_integer(name, default)
    }
}

impl FileGcSink for TdStorageEnv<'_> {
    fn use_file_database(&self) -> bool {
        self.shared.lock().use_file_database()
    }

    fn on_file_unlink(&mut self, location: FullLocalFileLocation) {
        let shared = self.shared.lock();
        shared.file_manager.lock().on_file_unlink(&location);
    }
}

impl FileStatsSink for TdStorageEnv<'_> {
    fn database_paths(&self) -> Vec<String> {
        self.shared.lock().database_paths()
    }

    fn language_pack_database_path(&self) -> String {
        self.language_pack_database_path.clone()
    }

    /// `scan_db` (FileStatsWorker.cpp:39-94) — the `file0`..`file:` range
    /// join over the "files" kv: the `@@` refs skip, the FileData parse
    /// carries the local path/type, the owner and the size.
    fn scan_file_db(&mut self, callback: &mut dyn FnMut(FileType, String, DialogId, i64)) {
        let (files_dir, secure_files_dir) = {
            let shared = self.shared.lock();
            (shared.files_dir(), shared.secure_files_dir())
        };
        self.global.td_db().files_kv().get_by_range(
            b"file0",
            b"file:",
            &mut |key: &[u8], value: &[u8]| {
                if !key.starts_with(b"file") {
                    return true;
                }
                // skip reference to other data
                if value.starts_with(b"@@") {
                    return true;
                }
                let mut parser = crate::store_parse::Parser::new(value);
                let data = match crate::files::FileData::parse_from(&mut parser, false, None) {
                    Ok(data) => data,
                    Err(_) => return true, // "Invalid FileData in the database"
                };
                let (file_type, path) = match &data.local {
                    crate::files::LocalFileLocation::Full(full) => {
                        (full.file_type, full.path.clone())
                    }
                    crate::files::LocalFileLocation::Partial(partial) => {
                        (partial.file_type, partial.path.clone())
                    }
                    _ => return true,
                };
                let path = if std::path::Path::new(&path).is_relative() {
                    format!(
                        "{}{}",
                        crate::files::get_files_base_dir(file_type, &files_dir, &secure_files_dir),
                        path
                    )
                } else {
                    path
                };
                let size = data.size;
                if size == 0 && matches!(data.local, crate::files::LocalFileLocation::Full(_)) {
                    // "Unknown size in the database"
                    return true;
                }
                callback(file_type, path, data.owner_dialog_id, size);
                true
            },
        );
    }
}

impl StorageContext for TdStorageEnv<'_> {
    fn pmc_get(&mut self, key: &str) -> Vec<u8> {
        self.global.binlog_pmc().get_raw(key.as_bytes())
    }

    fn pmc_set(&mut self, key: &str, value: &[u8]) {
        self.global.binlog_pmc().set_raw(key.as_bytes(), value);
    }

    fn use_storage_optimizer(&self) -> bool {
        self.shared.lock().use_storage_optimizer()
    }

    fn use_file_database(&self) -> bool {
        self.shared.lock().use_file_database()
    }

    fn database_stats(&mut self) -> Result<String, tdutils::status::Status> {
        match self.global.td_db().get_stats() {
            tdutils::status::Result::Ok(stats) => Ok(stats),
            tdutils::status::Result::Err(status) => Err(status),
        }
    }
}

/// The `FileGcSink` over the shared files plane alone — `run_gc` takes
/// the context and the sink as separate borrows, and the sink only needs
/// the FileManager unlink hop and the `use_file_database` switch.
struct TdFileGcSink(Arc<parking_lot::Mutex<TdFileShared>>);

impl FileGcSink for TdFileGcSink {
    fn use_file_database(&self) -> bool {
        self.0.lock().use_file_database()
    }

    fn on_file_unlink(&mut self, location: FullLocalFileLocation) {
        let file_manager = Arc::clone(&self.0.lock().file_manager);
        file_manager.lock().on_file_unlink(&location);
    }
}

/// Build the [`TdStorageEnv`] over disjoint Td field borrows (the
/// `UserEnv` convention — the manager lives in another field).
fn storage_env_fields<'a>(
    global: &'a mut Global,
    file_shared: &Arc<parking_lot::Mutex<TdFileShared>>,
    options: Option<&OptionManager>,
) -> TdStorageEnv<'a> {
    TdStorageEnv {
        shared: Arc::clone(file_shared),
        global,
        language_pack_database_path: options
            .map(|options| options.get_option_string("language_pack_database_path", ""))
            .unwrap_or_default(),
    }
}

// ── the live net seams (T.9) ──
//
// The bridge-backed nets: every query rides `BridgeHandle::query` (the
// established S76 blocking-invoke over the single bridge session; the
// gzip_packed responses are unwrapped by the session layer) and the io
// rides the FileLoaderUtils helpers over the per-drive files dirs. When
// the bridge is not installed yet (pre-init drives) the nets answer the
// session-down surface — the C++ parks the queries until a session
// appears, so nothing progresses offline either way.

/// The shared live-net environment: the bridge handle (filled at `init`,
/// once the session exists) and the files snapshot cell (the io dirs and
/// the file-name counters refresh per drive).
#[derive(Default)]
pub(crate) struct LiveNetCell {
    bridge: Option<crate::net::BridgeHandle>,
    shared: Option<Arc<parking_lot::Mutex<TdFileShared>>>,
}

impl LiveNetCell {
    fn install(
        &mut self,
        bridge: crate::net::BridgeHandle,
        shared: Arc<parking_lot::Mutex<TdFileShared>>,
    ) {
        self.bridge = Some(bridge);
        self.shared = Some(shared);
    }

    /// The bridge handle clone (the cell keeps the canonical one).
    fn bridge(&self) -> Option<crate::net::BridgeHandle> {
        self.bridge.clone()
    }

    /// One query through the bridge as a `Status` result (the C++
    /// NetQuery error surfaces: the RPC error code/message).
    fn query(&self, function: telegram_api::Function) -> Result<Vec<u8>, Status> {
        match self
            .bridge()
            .ok_or_else(|| Status::error(400, "Not connected"))?
            .query(function)
        {
            crate::net::QueryAnswer::Ok(data) => Ok(data),
            crate::net::QueryAnswer::Err(code, message) => Err(Status::error(code, &message)),
        }
    }

    /// Run `f` against the shared files snapshot.
    fn with_shared<R>(&self, f: impl FnOnce(&mut TdFileShared) -> R) -> Result<R, Status> {
        let Some(shared) = self.shared.as_ref() else {
            return Err(Status::error_message("The files snapshot is not installed"));
        };
        Ok(f(&mut shared.lock()))
    }

    /// The per-drive files dirs.
    fn io_dirs(&self) -> Result<(String, String), Status> {
        self.with_shared(|shared| (shared.files_dir(), shared.secure_files_dir()))
    }
}

/// The per-query download net over the bridge (`upload.getFile` family +
/// the FileLoaderUtils io).
pub(crate) struct LiveDownloadNet {
    cell: Arc<parking_lot::Mutex<LiveNetCell>>,
}

impl FileDownloaderNet for LiveDownloadNet {
    fn download(
        &mut self,
        query: DownloadQuery,
        _query_type: DownloadQueryType,
        _meta: DownloadNetMeta,
    ) -> Result<DownloadResult, Status> {
        let cell = self.cell.lock();
        let data = match query {
            DownloadQuery::GetFile {
                location,
                offset,
                limit,
                cdn_supported,
            } => {
                let function =
                    telegram_api::Function::upload_getFile(telegram_api::upload_getFile {
                        flags: 0,
                        precise: false,
                        cdn_supported,
                        location: Some(location),
                        offset,
                        limit,
                    });
                let data = cell.query(function)?;
                match telegram_api::upload_File::parse(&data) {
                    Ok(telegram_api::upload_File::upload_file(file)) => {
                        DownloadResult::Bytes(file.bytes)
                    }
                    Ok(telegram_api::upload_File::upload_fileCdnRedirect(redirect)) => {
                        DownloadResult::CdnRedirect {
                            dc_id: redirect.dc_id,
                            file_token: redirect.file_token,
                            encryption_key: redirect.encryption_key,
                            encryption_iv: redirect.encryption_iv,
                            file_hashes: redirect.file_hashes,
                        }
                    }
                    Err(error) => {
                        return Err(Status::error_message(&format!("upload.file: {error}")));
                    }
                }
            }
            DownloadQuery::GetWebFile {
                location,
                offset,
                limit,
            } => {
                let function =
                    telegram_api::Function::upload_getWebFile(telegram_api::upload_getWebFile {
                        location: Some(location),
                        offset,
                        limit,
                    });
                let data = cell.query(function)?;
                match telegram_api::upload_WebFile::parse(&data) {
                    Ok(web_file) => DownloadResult::Bytes(web_file.bytes),
                    Err(error) => {
                        return Err(Status::error_message(&format!("upload.webFile: {error}")));
                    }
                }
            }
            DownloadQuery::GetCdnFile {
                file_token,
                offset,
                limit,
            } => {
                let function =
                    telegram_api::Function::upload_getCdnFile(telegram_api::upload_getCdnFile {
                        file_token,
                        offset,
                        limit,
                    });
                let data = cell.query(function)?;
                match telegram_api::upload_CdnFile::parse(&data) {
                    Ok(telegram_api::upload_CdnFile::upload_cdnFileReuploadNeeded(needed)) => {
                        DownloadResult::ReuploadNeeded {
                            request_token: needed.request_token,
                        }
                    }
                    Ok(telegram_api::upload_CdnFile::upload_cdnFile(file)) => {
                        DownloadResult::CdnBytes(file.bytes)
                    }
                    Err(error) => {
                        return Err(Status::error_message(&format!("upload.cdnFile: {error}")));
                    }
                }
            }
            DownloadQuery::ReuploadCdnFile {
                file_token,
                request_token,
            } => {
                let function = telegram_api::Function::upload_reuploadCdnFile(
                    telegram_api::upload_reuploadCdnFile {
                        file_token,
                        request_token,
                    },
                );
                let data = cell.query(function)?;
                match parse_vector_file_hash(&data) {
                    Ok(hashes) => DownloadResult::Hashes(hashes),
                    Err(error) => {
                        return Err(Status::error_message(&format!(
                            "upload.reuploadCdnFile: {error}"
                        )));
                    }
                }
            }
            DownloadQuery::GetFileHashes { location, offset } => {
                let function = telegram_api::Function::upload_getFileHashes(
                    telegram_api::upload_getFileHashes {
                        location: Some(location),
                        offset,
                    },
                );
                let data = cell.query(function)?;
                match parse_vector_file_hash(&data) {
                    Ok(hashes) => DownloadResult::Hashes(hashes),
                    Err(error) => {
                        return Err(Status::error_message(&format!(
                            "upload.getFileHashes: {error}"
                        )));
                    }
                }
            }
        };
        Ok(data)
    }

    fn open_temp_file(&mut self, file_type: FileType) -> Result<(std::fs::File, String), Status> {
        self.cell
            .lock()
            .with_shared(|shared| shared.open_temp_file(file_type))?
    }

    fn open_partial_file(&mut self, path: &str, write: bool) -> Result<std::fs::File, Status> {
        crate::files::open_partial_file_impl(path, write)
    }

    fn open_generated_file(&mut self, path: &str) -> Result<std::fs::File, Status> {
        // `FileFd::open(path_, FileFd::Create | FileFd::Write)` — the
        // generated path may have just been unlinked (a stale partial)
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|e| Status::error_message(&format!("Can't open the generated file: {e}")))
    }

    fn search_file(
        &mut self,
        file_type: FileType,
        name: &str,
        size: i64,
    ) -> Result<String, Status> {
        let (files_dir, secure_files_dir) = self.cell.lock().io_dirs()?;
        crate::files::search_file_impl(file_type, name, size, &files_dir, &secure_files_dir)
    }

    fn create_from_temp(
        &mut self,
        file_type: FileType,
        path: &str,
        name: &str,
    ) -> Result<String, Status> {
        self.cell
            .lock()
            .with_shared(|shared| shared.create_from_temp(file_type, path, name))?
    }

    fn webfile_dc_id(&mut self) -> DcId {
        self.cell
            .lock()
            .with_shared(|shared| {
                let id =
                    shared.option_integer("webfile_dc_id", if shared.is_test_dc() { 2 } else { 4 });
                DcId::internal(id as i32)
            })
            .unwrap_or_default()
    }
}

/// The per-query upload net over the bridge (`upload.saveFilePart` /
/// `upload.saveBigFilePart`).
pub(crate) struct LiveUploadNet {
    cell: Arc<parking_lot::Mutex<LiveNetCell>>,
}

impl FileUploaderNet for LiveUploadNet {
    fn upload_part(&mut self, query: UploadPartQuery) -> Result<bool, Status> {
        let function = match query {
            UploadPartQuery::SaveFilePart {
                file_id,
                part_id,
                bytes,
            } => telegram_api::Function::upload_saveFilePart(telegram_api::upload_saveFilePart {
                file_id,
                file_part: part_id,
                bytes,
            }),
            UploadPartQuery::SaveBigFilePart {
                file_id,
                part_id,
                part_count,
                bytes,
            } => telegram_api::Function::upload_saveBigFilePart(
                telegram_api::upload_saveBigFilePart {
                    file_id,
                    file_part: part_id,
                    file_total_parts: part_count,
                    bytes,
                },
            ),
        };
        let data = self.cell.lock().query(function)?;
        // `!result.ok()` is the C++ 500 path (FileUploader.cpp:188-192);
        // the `Bool` result is the id-checked bare constructor
        let mut reader = telegram_api::tl::Reader::new(&data);
        match reader.bool() {
            Ok(ok) => Ok(ok),
            Err(error) => Err(Status::error_message(&format!(
                "upload.saveFilePart: {error}"
            ))),
        }
    }
}

/// The per-query hash-upload net over the bridge
/// (`messages.getDocumentByHash`).
pub(crate) struct LiveHashNet {
    cell: Arc<parking_lot::Mutex<LiveNetCell>>,
}

impl FileHashUploaderNet for LiveHashNet {
    fn get_document_by_hash(
        &mut self,
        sha256: [u8; 32],
        size: i64,
        mime_type: String,
    ) -> Result<DocumentByHash, Status> {
        let function = telegram_api::Function::messages_getDocumentByHash(
            telegram_api::messages_getDocumentByHash {
                sha256: sha256.to_vec(),
                size,
                mime_type,
            },
        );
        let data = self.cell.lock().query(function)?;
        match telegram_api::Document::parse(&data) {
            Ok(telegram_api::Document::documentEmpty(_)) => Ok(DocumentByHash::Empty),
            Ok(telegram_api::Document::document(document)) => Ok(DocumentByHash::Document {
                id: document.id,
                access_hash: document.access_hash,
                dc_id: document.dc_id,
                file_reference: document.file_reference,
            }),
            Err(error) => Err(Status::error_message(&format!(
                "messages.getDocumentByHash: {error}"
            ))),
        }
    }
}

/// `Vector<fileHash>` (the `upload.getFileHashes`/`upload.reuploadCdnFile`
/// result).
fn parse_vector_file_hash(data: &[u8]) -> Result<Vec<telegram_api::FileHash>, &'static str> {
    let mut reader = telegram_api::tl::Reader::new(data);
    if reader.i32()? != telegram_api::tl::VECTOR_ID {
        return Err("not a vector");
    }
    let count = reader.i32()? as u32 as usize;
    let mut hashes = Vec::with_capacity(count);
    for _ in 0..count {
        // vector elements are bare (no constructor id)
        hashes.push(telegram_api::FileHash::read_bare(&mut reader)?);
    }
    Ok(hashes)
}

/// The [`FileLoadNetFactory`] over the live bridge-backed nets.
pub(crate) struct TdFileLoadNetFactory {
    cell: Arc<parking_lot::Mutex<LiveNetCell>>,
}

impl TdFileLoadNetFactory {
    pub(crate) fn new() -> TdFileLoadNetFactory {
        TdFileLoadNetFactory {
            cell: Arc::new(parking_lot::Mutex::new(LiveNetCell::default())),
        }
    }

    /// Fills the cell once the session exists (Td `init`).
    pub(crate) fn install(
        &self,
        bridge: crate::net::BridgeHandle,
        shared: Arc<parking_lot::Mutex<TdFileShared>>,
    ) {
        self.cell.lock().install(bridge, shared);
    }
}

impl FileLoadNetFactory for TdFileLoadNetFactory {
    fn download_net(&mut self) -> Box<dyn FileDownloaderNet> {
        Box::new(LiveDownloadNet {
            cell: Arc::clone(&self.cell),
        })
    }

    fn upload_net(&mut self) -> Box<dyn FileUploaderNet> {
        Box::new(LiveUploadNet {
            cell: Arc::clone(&self.cell),
        })
    }

    fn hash_net(&mut self) -> Box<dyn FileHashUploaderNet> {
        Box::new(LiveHashNet {
            cell: Arc::clone(&self.cell),
        })
    }

    fn utils_net(&mut self) -> Box<dyn FileDownloaderNet> {
        self.download_net()
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
        // The WebPagesManager pending-page timeouts (M.17): the
        // collapsed MultiTimeout fires each due id once.
        {
            let now = tdutils::time::now();
            let due: Vec<i64> = self
                .web_pages_timeouts
                .iter()
                .filter(|(_, deadline)| **deadline <= now)
                .map(|(&key, _)| key)
                .collect();
            for web_page_id in due {
                self.web_pages_timeouts.remove(&web_page_id);
                let (mut env, web_pages_manager) = web_pages_env_split(self);
                web_pages_manager.on_pending_web_page_timeout(&mut env, web_page_id);
            }
        }
        // The InlineQueriesManager timeout sweep (M.21): the C++
        // `on_pending_inline_query_timeout_callback` collapses into
        // `tick_timeouts`.
        {
            let (mut env, inline_queries_manager, _) = inline_env_split(self);
            inline_queries_manager.tick_timeouts(&mut env);
        }
        // The MessageQueryManager timeout families (M.18): the C++
        // `send_message_view_metrics_timeout_callback` and the reload
        // SleepActor collapse into `tick_timeouts`.
        {
            let mut env = MessageQueryEnv {
                global: &mut self.global,
                options: self.option_manager.as_mut(),
                auth_is_authorized: self.auth_manager.is_authorized(),
                auth_is_bot: self.auth_manager.is_bot(),
                can_ignore_background_updates: self.can_ignore_background_updates,
                updates: &mut self.message_query_updates,
                user_manager: &mut self.user_manager,
                updates_manager: &mut self.updates_manager,
            };
            self.message_query_manager.tick_timeouts(&mut env);
        }
        // The PollManager's three MultiTimeouts (M.19): the C++
        // `on_*_poll_timeout_callback`s collapse into `tick_timeouts`.
        {
            let (mut env, poll_manager) = poll_env_split(self);
            poll_manager.tick_timeouts(&mut env);
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
        // The StorageManager's auto-gc timeout (T.8): the C++ actor's own
        // timer, collapsed into the shared slot's fire — before the flush
        // so the gc's unlink updates ride the same drive.
        self.tick_storage_gc();
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

/// Field-split the `Td` for the PollManager calls: the env borrows the
/// manager surfaces, the split returns the poll manager itself.
pub(crate) fn poll_env_split(
    td: &mut Td,
) -> (
    crate::poll_manager::PollManagerEnvImpl<'_>,
    &mut crate::poll_manager::PollManager,
) {
    let crate::td::Td {
        global,
        option_manager,
        auth_manager,
        user_manager,
        chat_manager,
        file_manager,
        poll_manager,
        poll_updates,
        ..
    } = td;
    (
        crate::poll_manager::PollManagerEnvImpl {
            global,
            options: option_manager.as_mut(),
            auth_is_bot: auth_manager.is_bot(),
            user_manager,
            chat_manager,
            file_manager: std::sync::Arc::clone(file_manager),
            updates: poll_updates,
        },
        poll_manager,
    )
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

/// The [`crate::people_nearby_manager::PeopleNearbyManagerEnv`] over the
/// `Td` fields — the collapsed constructor surface (built per call, never
/// held across calls).
pub(crate) struct PeopleNearbyEnv<'a> {
    pub(crate) is_bot: bool,
    pub(crate) global: &'a mut Global,
}

impl crate::people_nearby_manager::PeopleNearbyManagerEnv for PeopleNearbyEnv<'_> {
    fn is_bot(&self) -> bool {
        self.is_bot
    }

    fn pmc_erase(&mut self, key: &str) {
        self.global.binlog_pmc().erase(key);
    }
}

/// The [`crate::web_browser_manager::WebBrowserManagerEnv`] over the `Td`
/// fields — built per call with the field-split borrows (the
/// `NotificationEnv` convention), never held across calls.
pub(crate) struct WebBrowserEnv<'a> {
    pub(crate) global: &'a mut Global,
    pub(crate) auth_is_authorized: bool,
    pub(crate) auth_is_bot: bool,
    pub(crate) updates: &'a mut Vec<td_api::Update>,
}

impl crate::web_browser_manager::WebBrowserManagerEnv for WebBrowserEnv<'_> {
    fn is_authorized(&self) -> bool {
        self.auth_is_authorized
    }

    fn is_bot(&self) -> bool {
        self.auth_is_bot
    }

    fn pmc_get_raw(&mut self, key: &[u8]) -> Vec<u8> {
        self.global.binlog_pmc().get_raw(key)
    }

    fn pmc_set_raw(&mut self, key: &[u8], value: &[u8]) {
        self.global.binlog_pmc().set_raw(key, value);
    }

    fn send_update(&mut self, update: td_api::Update) {
        self.updates.push(update);
    }
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

impl crate::photo::PhotoStoreEnv for crate::poll_manager::PollManagerEnvImpl<'_> {
    fn store_file_id(&mut self, file_id: crate::files::FileId, out: &mut Vec<u8>) {
        self.file_manager.lock().store_file(file_id, out);
    }

    fn parse_file_id(
        &mut self,
        parser: &mut crate::store_parse::Parser<'_>,
    ) -> Result<crate::files::FileId, &'static str> {
        self.file_manager.lock().parse_file(parser)
    }
}

impl crate::poll_manager::PollStoreEnv for crate::poll_manager::PollManagerEnvImpl<'_> {
    /// `store_message_content` — [blocked: M.9 integration]: no code path
    /// on this line can set poll media, so the codec never runs.
    fn store_poll_media(&mut self, _media_type: i32, _out: &mut Vec<u8>) {}

    /// `parse_message_content` — [blocked: M.9 integration]: db records
    /// carrying poll media fail the parse, deferring to the
    /// message-content line.
    fn parse_poll_media(
        &mut self,
        _parser: &mut crate::store_parse::Parser<'_>,
    ) -> Result<i32, &'static str> {
        Err("Poll media parse — [blocked: M.9 integration]")
    }
}

impl crate::poll_manager::PollManagerEnv for crate::poll_manager::PollManagerEnvImpl<'_> {
    fn auth_is_bot(&self) -> bool {
        self.auth_is_bot
    }

    fn close_flag(&self) -> bool {
        self.global.close_flag()
    }

    fn use_message_database(&self) -> bool {
        self.global.use_message_database()
    }

    fn unix_time(&self) -> i32 {
        self.global.unix_time()
    }

    fn server_time(&self) -> f64 {
        self.global.server_time()
    }

    fn now(&self) -> f64 {
        tdutils::time::now()
    }

    fn is_test_dc(&self) -> bool {
        self.global.is_test_dc()
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

    fn binlog_rewrite(&mut self, log_event_id: u64, handler_type: i32, data: &[u8]) -> u64 {
        self.global
            .td_db()
            .binlog()
            .rewrite(log_event_id, handler_type, data)
    }

    fn binlog_erase(&mut self, log_event_id: u64) {
        self.global.td_db().binlog().erase(log_event_id);
    }

    fn add_min_channel(
        &mut self,
        channel_id: crate::chat_manager::ChannelId,
        min_channel: crate::chat_manager::MinChannel,
    ) {
        self.chat_manager.add_min_channel(channel_id, min_channel);
    }

    fn get_min_channel(
        &self,
        channel_id: crate::chat_manager::ChannelId,
    ) -> Option<crate::chat_manager::MinChannel> {
        self.chat_manager.get_min_channel(channel_id).cloned()
    }

    fn channel_is_member(&self, channel_id: crate::chat_manager::ChannelId) -> bool {
        self.chat_manager.get_channel_status(channel_id).is_member()
    }

    /// `chat_manager_->get_channel_date` — the collapsed line answers 0;
    /// the subscribers-only membership window rides the M.5 integration.
    fn channel_date(&self, _channel_id: crate::chat_manager::ChannelId) -> i32 {
        0
    }

    /// `dialog_manager_->have_dialog_info` — the user half is real; the
    /// chat/channel halves are the M.6 stub inventory.
    fn have_dialog_info(&self, dialog_id: crate::dialog_id::DialogId) -> bool {
        use crate::dialog_id::DialogType;
        match dialog_id.get_type() {
            DialogType::User => self
                .user_manager
                .have_accessible_user(dialog_id.get_user_id()),
            DialogType::Chat | DialogType::Channel => false,
            DialogType::SecretChat => self
                .user_manager
                .have_secret_chat(dialog_id.get_secret_chat_id()),
            DialogType::None => false,
        }
    }

    fn have_min_user(&self, user_id: crate::user_manager::UserId) -> bool {
        self.user_manager.have_min_user(user_id)
    }

    fn get_my_id(&self) -> crate::user_manager::UserId {
        self.user_manager.get_my_id()
    }

    fn get_my_dialog_id(&self) -> crate::dialog_id::DialogId {
        crate::dialog_id::DialogId::from_user_id(self.user_manager.get_my_id())
    }

    fn get_option_integer(&self, name: &str, default_value: i64) -> i64 {
        self.options.as_ref().map_or(default_value, |options| {
            options.get_option_integer(name, default_value)
        })
    }

    fn get_option_string(&self, name: &str) -> String {
        self.options
            .as_ref()
            .map_or_else(String::new, |options| options.get_option_string(name, ""))
    }

    /// `messages_manager_->get_message_poll_id` — [blocked: M.10
    /// integration]: the seam answers the not-found band.
    fn get_message_poll_id(
        &mut self,
        _message_full_id: &crate::dialog_id::MessageFullId,
        _ignore_has_poll: bool,
    ) -> Result<crate::poll_manager::PollId, tdutils::status::Status> {
        Err(tdutils::status::Status::error(400, "Chat not found"))
    }

    /// `messages_manager_->has_message_sender_user_id` — the M.10 seam.
    fn has_message_sender_user_id(
        &mut self,
        _message_full_id: &crate::dialog_id::MessageFullId,
    ) -> bool {
        false
    }

    /// `messages_manager_->on_external_update_message_content` — the M.10
    /// seam no-op (the notify fan-out rides the message plane).
    fn on_external_update_message_content(
        &mut self,
        _message_full_id: crate::dialog_id::MessageFullId,
    ) {
    }

    /// `messages_manager_->on_update_poll_has_unread_votes` — the M.10
    /// seam no-op.
    fn on_update_poll_has_unread_votes(
        &mut self,
        _message_full_id: crate::dialog_id::MessageFullId,
        _has_unread_votes: bool,
    ) {
    }

    /// `web_pages_manager_->register_poll_web_pages` — the M.17 seam
    /// no-op (the WebPageId newtype lands with the web-pages line).
    fn register_poll_web_pages(
        &mut self,
        _poll_id: crate::poll_manager::PollId,
        _web_page_ids: Vec<i64>,
    ) {
    }

    /// `get_min_message_sender_object` — the user half is real; the
    /// chat/channel halves ride the M.6 line.
    fn get_min_message_sender_object(
        &mut self,
        dialog_id: crate::dialog_id::DialogId,
    ) -> Option<td_api::MessageSender> {
        use crate::dialog_id::DialogType;
        match dialog_id.get_type() {
            DialogType::User
                if self
                    .user_manager
                    .have_accessible_user(dialog_id.get_user_id()) =>
            {
                Some(td_api::MessageSender::messageSenderUser(
                    td_api::messageSenderUser {
                        user_id: dialog_id.get_user_id().get(),
                    },
                ))
            }
            _ => None,
        }
    }

    /// `get_poll_media_object` — [blocked: M.9 integration]: the
    /// option/explanation media objects land with the message-content
    /// line; `None` is the null media of the C++ call.
    fn get_poll_media_object(&mut self, _media_type: Option<i32>) -> Option<td_api::PollMedia> {
        None
    }

    fn send_update(&mut self, update: td_api::Update) {
        self.updates.push(update);
    }

    fn get_formatted_text_with_users(
        &mut self,
        text: String,
        entities: Vec<tdmtproto::telegram_api::MessageEntity>,
        skip_media_timestamps: bool,
        skip_trim: bool,
        source: &str,
    ) -> crate::message_entity::FormattedText {
        crate::message_entity::get_formatted_text::<()>(
            None,
            text,
            entities,
            skip_media_timestamps,
            skip_trim,
            source,
        )
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

impl crate::transcription_manager::TranscriptionManagerEnv for NotificationEnv<'_> {
    fn auth_is_authorized(&self) -> bool {
        self.auth_is_authorized
    }

    fn auth_is_bot(&self) -> bool {
        self.auth_is_bot
    }

    fn close_flag(&self) -> bool {
        self.global.close_flag()
    }

    fn unix_time(&self) -> i32 {
        self.global.unix_time()
    }

    fn now(&self) -> f64 {
        tdutils::time::now()
    }

    fn binlog_pmc_get_raw(&mut self, key: &[u8]) -> Vec<u8> {
        self.global.binlog_pmc().get_raw(key)
    }

    fn binlog_pmc_set_raw(&mut self, key: &[u8], value: &[u8]) {
        self.global.binlog_pmc().set_raw(key, value);
    }

    fn send_update(&mut self, update: td_api::Update) {
        self.updates.push(update);
    }

    /// `messages_manager_->have_message_force` — [blocked: M.10]; no
    /// message plane exists yet, so the probe answers the not-found band.
    fn have_message_force(
        &self,
        _message_full_id: crate::dialog_id::MessageFullId,
        _source: &str,
    ) -> bool {
        false
    }

    /// `messages_manager_->on_external_update_message_content` —
    /// [blocked: M.10].
    fn on_external_update_message_content(
        &mut self,
        message_full_id: crate::dialog_id::MessageFullId,
        source: &str,
    ) {
        tdutils::log_info!(
            "on_external_update_message_content {message_full_id:?} from {source} — [blocked: M.10]"
        );
    }

    /// `messages_manager_->on_update_message_content` — [blocked: M.10].
    fn on_update_message_content(&mut self, message_full_id: crate::dialog_id::MessageFullId) {
        tdutils::log_info!("on_update_message_content {message_full_id:?} — [blocked: M.10]");
    }

    /// `VideoNotesManager::get_video_note_transcription_info` /
    /// `VoiceNotesManager::get_voice_note_transcription_info` —
    /// [blocked: M.28]; the content plane owns no infos yet.
    fn with_transcription_info<
        R,
        F: FnOnce(&mut crate::transcription_manager::TranscriptionInfo) -> R,
    >(
        &mut self,
        file_info: crate::transcription_manager::FileInfo,
        _allow_creation: bool,
        _f: F,
    ) -> Option<R> {
        tdutils::log_info!("Transcription info for {file_info:?} — [blocked: M.28]");
        None
    }

    /// `TranscribeAudioQuery` — [blocked: net]; the query drops, the
    /// parked promises stay unanswered like the C++ offline.
    fn send_transcribe_audio_query(&mut self, message_full_id: crate::dialog_id::MessageFullId) {
        tdutils::log_info!("Transcribe audio {message_full_id:?} — [blocked: net]");
    }

    /// `RateTranscribedAudioQuery` — [blocked: net].
    fn send_rate_transcribed_audio_query(
        &mut self,
        message_full_id: crate::dialog_id::MessageFullId,
        transcription_id: i64,
        is_good: bool,
        _promise: tdutils::promise::Promise<()>,
    ) {
        tdutils::log_info!(
            "Rate transcribed audio {message_full_id:?} id {transcription_id} good {is_good} — [blocked: net]"
        );
    }
}

/// The [`crate::translation_manager::TranslationManagerEnv`] over the `Td`
/// fields — built per call (the `NotificationEnv` convention), never held
/// across calls. The user plane rides the same split-`UserEnv` borrow as
/// the bot arms (`bot_user_split`), because the manager's entity scan
/// also needs the [`crate::message_entity::UserManagerLike`] surface.
pub(crate) struct TranslationEnv<'a> {
    pub(crate) global: &'a mut Global,
    /// `None` once the option manager is torn down; the getters answer
    /// their defaults, matching the C++ close ordering.
    pub(crate) options: Option<&'a mut OptionManager>,
    pub(crate) auth_is_authorized: bool,
    pub(crate) auth_is_bot: bool,
    pub(crate) updates: &'a mut Vec<td_api::Update>,
    pub(crate) users: &'a mut UserManager,
}

impl TranslationEnv<'_> {
    /// The disjoint `UserEnv` + manager borrow (the `bot_user_split`
    /// convention in requests.rs).
    fn user_env_split(&mut self) -> (UserEnv<'_>, &mut UserManager) {
        let options = self
            .options
            .as_deref_mut()
            .expect("translation calls run with the initialized option manager");
        let env = UserEnv {
            global: &mut *self.global,
            options,
            auth_is_bot: self.auth_is_bot,
        };
        (env, &mut *self.users)
    }
}

impl crate::translation_manager::TranslationManagerEnv for TranslationEnv<'_> {
    fn auth_is_authorized(&self) -> bool {
        self.auth_is_authorized
    }

    fn auth_is_bot(&self) -> bool {
        self.auth_is_bot
    }

    fn close_flag(&self) -> bool {
        self.global.close_flag()
    }

    fn binlog_pmc_get_raw(&mut self, key: &[u8]) -> Vec<u8> {
        self.global.binlog_pmc().get_raw(key)
    }

    fn binlog_pmc_set_raw(&mut self, key: &[u8], value: &[u8]) {
        self.global.binlog_pmc().set_raw(key, value);
    }

    fn send_update(&mut self, update: td_api::Update) {
        self.updates.push(update);
    }

    fn on_get_users(&mut self, users: Vec<telegram_api::User>, source: &str) {
        let (mut env, user_manager) = self.user_env_split();
        user_manager.on_get_users(&mut env, users, source);
    }

    fn get_user_id_object(&mut self, user_id: i64, source: &str) -> i64 {
        let (mut env, users) = self.user_env_split();
        users.get_user_id_object(&mut env, UserId::new(user_id), source)
    }

    /// `Dependencies::add` × N + `Dependencies::resolve_force` —
    /// [blocked: the Dependencies owner]; like the
    /// `BotRecommendationManagerEnv` default, the collapse trusts the
    /// synchronous user cache (the tones answer re-intakes the authors
    /// through `on_get_users` once the net step lands).
    fn resolve_user_dependencies(&mut self, user_ids: &[i64], source: &str) -> bool {
        let _ = (user_ids, source);
        true
    }

    fn get_me(&mut self) {
        let (mut env, users) = self.user_env_split();
        users.get_me(&mut env);
    }

    /// `TranslateTextQuery::send` — [blocked: net]; the promise drops,
    /// which keeps the C++ request unanswered offline.
    fn send_translate_text_query(
        &mut self,
        texts: Vec<FormattedText>,
        to_language_code: String,
        tone: String,
        skip_bot_commands: bool,
        max_media_timestamp: i32,
        _promise: tdutils::promise::Promise<td_api::FormattedText>,
    ) {
        let _ = (
            texts,
            to_language_code,
            tone,
            skip_bot_commands,
            max_media_timestamp,
        );
        tdutils::log_info!("Translate text — [blocked: net]");
    }

    /// `ComposeMessageWithAiQuery::send` — [blocked: net].
    fn send_compose_message_with_ai_query(
        &mut self,
        text: FormattedText,
        translate_to_language_code: String,
        input_tone: Option<telegram_api::InputAiComposeTone>,
        emojify: bool,
        _promise: tdutils::promise::Promise<td_api::FormattedText>,
    ) {
        let _ = (text, translate_to_language_code, input_tone, emojify);
        tdutils::log_info!("Compose message with AI — [blocked: net]");
    }

    /// `ProofreadMessageWithAiQuery::send` — [blocked: net].
    fn send_proofread_message_with_ai_query(
        &mut self,
        text: FormattedText,
        _promise: tdutils::promise::Promise<td_api::FixedText>,
    ) {
        let _ = text;
        tdutils::log_info!("Proofread message with AI — [blocked: net]");
    }

    /// `GetAiComposeTonesQuery::send` — [blocked: net].
    fn send_get_ai_compose_tones_query(
        &mut self,
        hash: i64,
        _promise: tdutils::promise::Promise<()>,
    ) {
        let _ = hash;
        tdutils::log_info!("Get AI-compose tones hash {hash} — [blocked: net]");
    }

    /// `CreateToneQuery::send` — [blocked: net].
    fn send_create_tone_query(
        &mut self,
        title: String,
        custom_emoji_id: i64,
        prompt: String,
        show_creator: bool,
        _promise: tdutils::promise::Promise<td_api::TextCompositionStyle>,
    ) {
        let _ = (title, custom_emoji_id, prompt, show_creator);
        tdutils::log_info!("Create AI-compose tone — [blocked: net]");
    }

    /// `UpdateToneQuery::send` — [blocked: net].
    fn send_update_tone_query(
        &mut self,
        input_tone: telegram_api::InputAiComposeTone,
        title: String,
        custom_emoji_id: i64,
        prompt: String,
        show_creator: bool,
        _promise: tdutils::promise::Promise<td_api::TextCompositionStyle>,
    ) {
        let _ = (input_tone, title, custom_emoji_id, prompt, show_creator);
        tdutils::log_info!("Update AI-compose tone — [blocked: net]");
    }

    /// `DeleteToneQuery::send` — [blocked: net].
    fn send_delete_tone_query(
        &mut self,
        input_tone: telegram_api::InputAiComposeTone,
        _promise: tdutils::promise::Promise<()>,
    ) {
        let _ = input_tone;
        tdutils::log_info!("Delete AI-compose tone — [blocked: net]");
    }

    /// `GetToneQuery::send` — [blocked: net].
    fn send_get_tone_query(
        &mut self,
        input_tone: telegram_api::InputAiComposeTone,
        _promise: tdutils::promise::Promise<td_api::TextCompositionStyle>,
    ) {
        let _ = input_tone;
        tdutils::log_info!("Get AI-compose tone — [blocked: net]");
    }

    /// `GetToneExampleQuery::send` — [blocked: net].
    fn send_get_tone_example_query(
        &mut self,
        input_tone: telegram_api::InputAiComposeTone,
        num: i32,
        _promise: tdutils::promise::Promise<td_api::TextCompositionStyleExample>,
    ) {
        let _ = (input_tone, num);
        tdutils::log_info!("Get AI-compose tone example — [blocked: net]");
    }

    /// `SaveToneQuery::send` — [blocked: net].
    fn send_save_tone_query(
        &mut self,
        input_tone: telegram_api::InputAiComposeTone,
        unsave: bool,
        _promise: tdutils::promise::Promise<()>,
    ) {
        let _ = (input_tone, unsave);
        tdutils::log_info!("Save AI-compose tone — [blocked: net]");
    }

    /// `TranslateRichMessageQuery::send` — [blocked: net] over the
    /// [blocked: RichMessage] value plane; the promise drops unanswered.
    fn send_translate_rich_message_query(
        &mut self,
        message: td_api::InputRichMessage,
        to_language_code: String,
        tone: String,
        _promise: tdutils::promise::Promise<td_api::RichMessage>,
    ) {
        let _ = (message, to_language_code, tone);
        tdutils::log_info!("Translate rich message — [blocked: RichMessage]");
    }

    /// `do_compose_rich_message_with_ai` +
    /// `ComposeRichMessageWithAiQuery::send` — [blocked: net] over the
    /// [blocked: RichMessage] value plane; the promise drops unanswered.
    fn send_compose_rich_message_with_ai_query(
        &mut self,
        has_message: bool,
        message: Option<td_api::InputRichMessage>,
        translate_to_language_code: String,
        input_tone: Option<telegram_api::InputAiComposeTone>,
        emojify: bool,
        proofread: bool,
        _promise: tdutils::promise::Promise<td_api::RichMessage>,
    ) {
        let _ = (
            has_message,
            message,
            translate_to_language_code,
            input_tone,
            emojify,
            proofread,
        );
        tdutils::log_info!("Compose rich message with AI — [blocked: RichMessage]");
    }
}

impl crate::message_entity::UserManagerLike for TranslationEnv<'_> {
    fn resolve_user_id_object(&mut self, user_id: UserId, source: &str) -> i64 {
        let (mut env, users) = self.user_env_split();
        users.get_user_id_object(&mut env, user_id, source)
    }

    fn ensure_input_user(&mut self, user_id: UserId) -> Result<(), Status> {
        let (env, users) = self.user_env_split();
        users.get_input_user(&env, user_id).map(|_| ())
    }

    fn have_min_user(&self, user_id: UserId) -> bool {
        self.users.have_min_user(user_id)
    }

    fn get_input_user_force(&mut self, user_id: UserId) -> telegram_api::InputUser {
        let (env, users) = self.user_env_split();
        users.get_input_user_force(&env, user_id)
    }

    fn is_user_bot(&self, user_id: UserId) -> bool {
        self.users.is_user_bot_of(user_id)
    }
}

/// The option slice the collapsed LanguagePackManager reads through (the
/// C++ manager reads the same surfaces off its `G()`; the pmc/binlog
/// plane stays with the language pack database opened in
/// [`Td::init_language_pack_manager`]). The `send_update`s buffer into
/// [`Td::language_pack_updates`] and Td drains them — the collapsed
/// `send_closure(G()->td(), &Td::send_update)` hop. The five net queries
/// are the M.24 `[blocked: net]` seams: the C++ promises land in the
/// query (or the manager's pending maps) and stay unanswered offline.
pub(crate) struct LanguagePackEnv<'a> {
    pub(crate) options: Option<&'a mut OptionManager>,
    pub(crate) updates: &'a mut Vec<td_api::Update>,
}

impl crate::language_pack_manager::LanguagePackManagerEnv for LanguagePackEnv<'_> {
    fn get_option_string(&self, name: &str, default_value: &str) -> String {
        self.options.as_ref().map_or_else(
            || default_value.to_owned(),
            |options| options.get_option_string(name, default_value),
        )
    }

    fn get_option_integer(&self, name: &str, default_value: i64) -> i64 {
        self.options.as_ref().map_or(default_value, |options| {
            options.get_option_integer(name, default_value)
        })
    }

    fn set_option_empty(&mut self, name: &str) {
        if let Some(options) = self.options.as_mut() {
            options.set_option_empty(name);
        }
    }

    fn send_update(&mut self, update: td_api::Update) {
        self.updates.push(update);
    }

    /// `langpack_getLanguages` (LanguagePackManager.cpp:871-878).
    fn send_get_languages_query(
        &mut self,
        language_pack: String,
        _promise: tdutils::promise::Promise<td_api::LocalizationTargetInfo>,
    ) {
        tdutils::log_info!(
            "Send langpack.getLanguages for language pack {language_pack} — [blocked: net]"
        );
    }

    /// `langpack_getLanguage` (LanguagePackManager.cpp:887-897).
    fn send_get_language_query(
        &mut self,
        language_pack: String,
        language_code: String,
        _promise: tdutils::promise::Promise<td_api::LanguagePackInfo>,
    ) {
        tdutils::log_info!(
            "Send langpack.getLanguage for language pack {language_pack}, language code {language_code} — [blocked: net]"
        );
    }

    /// `langpack_getLangPack` (LanguagePackManager.cpp:1148-1150).
    fn send_get_lang_pack_query(&mut self, language_pack: String, language_code: String) {
        tdutils::log_info!(
            "Send langpack.getLangPack for language pack {language_pack}, language code {language_code} — [blocked: net]"
        );
    }

    /// `langpack_getStrings` (LanguagePackManager.cpp:1159-1161).
    fn send_get_strings_query(
        &mut self,
        language_pack: String,
        language_code: String,
        keys: Vec<String>,
        _promise: tdutils::promise::Promise<td_api::LanguagePackStrings>,
    ) {
        tdutils::log_info!(
            "Send langpack.getStrings for language pack {language_pack}, language code {language_code}, {} keys — [blocked: net]",
            keys.len()
        );
    }

    /// `langpack_getDifference` (LanguagePackManager.cpp:456-458).
    fn send_get_difference_query(
        &mut self,
        language_pack: String,
        language_code: String,
        from_version: i32,
    ) {
        tdutils::log_info!(
            "Send langpack.getDifference for language pack {language_pack}, language code {language_code}, from version {from_version} — [blocked: net]"
        );
    }
}

/// The `G()`/manager slice the collapsed MessageQueryManager reads through
/// (M.18) — the C++ manager reads the same surfaces off its `Td *td_`
/// directly. The `send_update`s buffer into [`Td::message_query_updates`]
/// and Td drains them; every net query tail and message-plane probe keeps
/// its default seam body (the honest error bands in
/// [`crate::message_query_manager::MessageQueryManagerEnv`]).
pub(crate) struct MessageQueryEnv<'a> {
    pub(crate) global: &'a mut Global,
    /// `None` once the option manager is torn down; the getters answer
    /// their defaults, matching the C++ close ordering.
    pub(crate) options: Option<&'a mut OptionManager>,
    pub(crate) auth_is_authorized: bool,
    pub(crate) auth_is_bot: bool,
    pub(crate) can_ignore_background_updates: bool,
    pub(crate) updates: &'a mut Vec<td_api::Update>,
    pub(crate) user_manager: &'a mut UserManager,
    pub(crate) updates_manager: &'a mut UpdatesManager<TdSink>,
}

impl crate::message_query_manager::MessageQueryManagerEnv for MessageQueryEnv<'_> {
    fn auth_is_bot(&self) -> bool {
        self.auth_is_bot
    }

    /// `Random::secure_int64` (td/utils/Random.h).
    fn secure_random_i64(&self) -> i64 {
        tdutils::random::secure_int64()
    }

    fn close_flag(&self) -> bool {
        self.global.close_flag()
    }

    fn unix_time(&self) -> i32 {
        self.global.unix_time()
    }

    fn now(&self) -> f64 {
        tdutils::time::now()
    }

    fn get_option_string(&self, name: &str, default_value: &str) -> String {
        self.options.as_ref().map_or_else(
            || default_value.to_owned(),
            |options| options.get_option_string(name, default_value),
        )
    }

    fn get_user_id_object(&mut self, user_id: UserId, source: &str) -> i64 {
        let global = &mut *self.global;
        let options = self
            .options
            .as_deref_mut()
            .expect("the option manager exists while the manager drives");
        let mut user_env = UserEnv {
            global,
            options,
            auth_is_bot: self.auth_is_bot,
        };
        self.user_manager
            .get_user_id_object(&mut user_env, user_id, source)
    }

    fn have_user_force(&mut self, user_id: UserId, source: &str) -> bool {
        let global = &mut *self.global;
        let options = self
            .options
            .as_deref_mut()
            .expect("the option manager exists while the manager drives");
        let mut user_env = UserEnv {
            global,
            options,
            auth_is_bot: self.auth_is_bot,
        };
        self.user_manager
            .have_user_force(&mut user_env, user_id, source)
    }

    fn send_update(&mut self, update: td_api::Update) {
        self.updates.push(update);
    }

    /// `updates_manager_->add_pending_pts_update` with the C++
    /// `dummyUpdate` — routed to the real engine (M.7).
    fn add_pending_pts_update(
        &mut self,
        pts: i32,
        pts_count: i32,
        receive_time: f64,
        promise: tdutils::promise::Promise<()>,
        source: &str,
    ) {
        let global = &mut *self.global;
        let options = self
            .options
            .as_deref_mut()
            .expect("the option manager exists while the updates engine drives");
        let mut updates_env = TdUpdatesEnv {
            global,
            options,
            auth_is_authorized: self.auth_is_authorized,
            auth_is_bot: self.auth_is_bot,
            my_id: self.user_manager.get_my_id().get(),
            can_ignore_background_updates: self.can_ignore_background_updates,
        };
        self.updates_manager.add_pending_pts_update(
            &mut updates_env,
            PendingUpdate::Dummy,
            pts,
            pts_count,
            receive_time,
            promise,
            source,
        );
    }
}

/// The [`crate::call_manager::CallManagerEnv`] over the `Td` fields —
/// built per call with the field-split borrows (the `NotificationEnv`
/// convention), never held across calls. The two call-notification seams
/// keep their no-op trait defaults until the notification env plumbing
/// joins the call plane (the M.7.12+ sink group).
pub(crate) struct CallEnv<'a> {
    pub(crate) user_env: UserEnv<'a>,
    pub(crate) user_manager: &'a mut UserManager,
    pub(crate) notification_manager: &'a mut NotificationManager,
    pub(crate) updates: &'a mut Vec<td_api::Update>,
    pub(crate) dh_config: &'a mut Option<DhConfig>,
}

impl crate::dh_cache::DhCacheEnv for CallEnv<'_> {
    fn pmc_get_raw(&mut self, key: &[u8]) -> Vec<u8> {
        self.user_env.global.binlog_pmc().get_raw(key)
    }

    fn pmc_set_raw(&mut self, key: &[u8], value: &[u8]) {
        self.user_env.global.binlog_pmc().set_raw(key, value);
    }
}

impl crate::call_actor::CallActorEnv for CallEnv<'_> {
    fn close_flag(&self) -> bool {
        self.user_env.global.close_flag()
    }

    fn get_option_integer(&self, name: &str, default_value: i64) -> i64 {
        self.user_env
            .options
            .get_option_integer(name, default_value)
    }

    fn get_dh_config(&self) -> Option<DhConfig> {
        self.dh_config.clone()
    }

    fn set_dh_config(&mut self, config: DhConfig) {
        *self.dh_config = Some(config);
    }

    fn on_get_users(&mut self, users: Vec<tdmtproto::telegram_api::User>, source: &str) {
        self.user_manager
            .on_get_users(&mut self.user_env, users, source);
    }

    fn get_input_user(
        &self,
        user_id: UserId,
    ) -> std::result::Result<tdmtproto::telegram_api::InputUser, tdutils::status::Status> {
        self.user_manager.get_input_user(&self.user_env, user_id)
    }

    fn get_user_id_object(&mut self, user_id: UserId) -> i64 {
        self.user_manager.get_user_id_object(
            &mut self.user_env,
            user_id,
            "CallActor::flush_call_state",
        )
    }

    fn add_call_notification(
        &mut self,
        dialog_id: crate::chat_manager::DialogId,
        call_id: crate::call_id::CallId,
    ) {
        let _ = (dialog_id, call_id);
        let _ = self.notification_manager;
    }

    fn remove_call_notification(
        &mut self,
        dialog_id: crate::chat_manager::DialogId,
        call_id: crate::call_id::CallId,
    ) {
        let _ = (dialog_id, call_id);
        let _ = self.notification_manager;
    }

    fn send_update(&mut self, update: td_api::Update) {
        self.updates.push(update);
    }
}

impl crate::call_manager::CallManagerEnv for CallEnv<'_> {
    fn close_status(&self) -> tdutils::status::Status {
        if self.user_env.global.close_flag() {
            crate::config_result::request_aborted_error()
        } else {
            tdutils::status::Status::ok()
        }
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

/// The account-plane env over the `Td` fields (AccountManager,
/// TermsOfServiceManager, SuggestedActionManager) — built per call with
/// the field-split borrows (the `NotificationEnv` convention).
pub(crate) struct AccountEnv<'a> {
    pub(crate) global: &'a mut Global,
    pub(crate) options: Option<&'a mut OptionManager>,
    pub(crate) user_manager: &'a mut UserManager,
    pub(crate) device_token_manager: &'a mut crate::device_token_manager::DeviceTokenManager,
    pub(crate) auth_is_bot: bool,
    pub(crate) updates: &'a mut Vec<td_api::Update>,
}

impl AccountManagerEnv for AccountEnv<'_> {
    fn close_flag(&self) -> bool {
        self.global.close_flag()
    }

    fn is_bot(&self) -> bool {
        self.auth_is_bot
    }

    fn get_option_integer(&self, name: &str, default_value: i64) -> i64 {
        self.options.as_ref().map_or(default_value, |options| {
            options.get_option_integer(name, default_value)
        })
    }

    fn get_option_string(&self, name: &str, default_value: &str) -> String {
        self.options.as_ref().map_or_else(
            || default_value.to_string(),
            |options| options.get_option_string(name, default_value),
        )
    }

    fn unix_time(&self) -> i32 {
        self.global.unix_time()
    }

    fn pmc_get_raw(&mut self, key: &[u8]) -> Vec<u8> {
        self.global.binlog_pmc().get_raw(key)
    }

    fn pmc_set_raw(&mut self, key: &[u8], value: &[u8]) {
        self.global.binlog_pmc().set_raw(key, value);
    }

    fn pmc_erase(&mut self, key: &str) {
        self.global.binlog_pmc().erase(key);
    }

    fn binlog_add(&mut self, handler_type: i32, data: &[u8]) -> u64 {
        self.global.td_db().binlog().add(handler_type, data)
    }

    fn send_update(&mut self, update: td_api::Update) {
        self.updates.push(update);
    }

    fn on_get_users(&mut self, users: Vec<tdmtproto::telegram_api::User>, source: &str) {
        let mut env = UserEnv {
            global: self.global,
            options: self
                .options
                .as_mut()
                .expect("account manager runs with the initialized option manager"),
            auth_is_bot: self.auth_is_bot,
        };
        self.user_manager.on_get_users(&mut env, users, source);
    }

    fn get_user_id_object(&mut self, user_id: i64, source: &str) -> i64 {
        let mut env = UserEnv {
            global: self.global,
            options: self
                .options
                .as_mut()
                .expect("account manager runs with the initialized option manager"),
            auth_is_bot: self.auth_is_bot,
        };
        self.user_manager
            .get_user_id_object(&mut env, UserId::new(user_id), source)
    }

    fn set_option_boolean(&mut self, name: &str, value: bool) {
        if let Some(options) = self.options.as_mut() {
            options.set_option_boolean(name, value);
        }
    }

    fn reregister_device(&mut self) {
        // the device-token env plumbing joins with the account plane
        // later (the M.13S continuation); the seam no-ops
        let _ = &mut self.device_token_manager;
    }
}

impl crate::terms_of_service_manager::TermsOfServiceManagerEnv for AccountEnv<'_> {
    fn send_update(&mut self, update: td_api::Update) {
        self.updates.push(update);
    }
}

impl crate::suggested_action_manager::SuggestedActionManagerEnv for AccountEnv<'_> {
    fn close_flag(&self) -> bool {
        self.global.close_flag()
    }

    fn pmc_get_raw(&mut self, key: &[u8]) -> Vec<u8> {
        self.global.binlog_pmc().get_raw(key)
    }

    fn pmc_set_raw(&mut self, key: &[u8], value: &[u8]) {
        self.global.binlog_pmc().set_raw(key, value);
    }

    fn pmc_erase(&mut self, key: &str) {
        self.global.binlog_pmc().erase(key);
    }

    fn get_option_string(&self, name: &str, default_value: &str) -> String {
        self.options.as_ref().map_or_else(
            || default_value.to_string(),
            |options| options.get_option_string(name, default_value),
        )
    }

    fn send_update(&mut self, update: td_api::Update) {
        self.updates.push(update);
    }
}

/// The community-plane env over the `Td` fields — built per call with the
/// field-split borrows (the `NotificationEnv` convention).
pub(crate) struct CommunityEnv<'a> {
    pub(crate) global: &'a mut Global,
    pub(crate) dialog_manager: &'a mut DialogManager,
    pub(crate) updates: &'a mut Vec<td_api::Update>,
}

impl CommunityManagerEnv for CommunityEnv<'_> {
    fn close_flag(&self) -> bool {
        self.global.close_flag()
    }

    fn send_update(&mut self, update: td_api::Update) {
        self.updates.push(update);
    }

    fn check_dialog_access(
        &mut self,
        dialog_id: crate::dialog_id::DialogId,
    ) -> std::result::Result<(), tdutils::status::Status> {
        // `DialogManager::check_dialog_access(dialog_id, false, Read,
        // "create_community")` — the full access-check plane is the M.6
        // continuation; the seam answers valid dialogs only.
        if dialog_id.is_valid() {
            Ok(())
        } else {
            Err(tdutils::status::Status::error(400, "Chat not found"))
        }
    }

    fn get_chat_id_object(&mut self, dialog_id: crate::dialog_id::DialogId) -> i64 {
        let _ = self.dialog_manager;
        dialog_id.get()
    }
}

/// The statistics-plane env over the `Td` fields.
pub(crate) struct StatisticsEnv<'a> {
    pub(crate) global: &'a mut Global,
    pub(crate) updates: &'a mut Vec<td_api::Update>,
}

impl StatisticsManagerEnv for StatisticsEnv<'_> {
    fn close_flag(&self) -> bool {
        self.global.close_flag()
    }

    fn send_update(&mut self, update: td_api::Update) {
        self.updates.push(update);
    }

    fn get_channel_statistics_dc_id(
        &mut self,
        dialog_id: crate::dialog_id::DialogId,
    ) -> std::result::Result<(), tdutils::status::Status> {
        // `ChatManager::get_channel_statistics_dc_id` — the dc-id answer
        // joins with the net step; the seam accepts valid dialogs.
        if dialog_id.is_valid() {
            Ok(())
        } else {
            Err(tdutils::status::Status::error(400, "Chat not found"))
        }
    }
}

/// The premium-plane env over the option manager.
pub(crate) struct PremiumTdEnv<'a> {
    pub(crate) options: Option<&'a mut OptionManager>,
}

impl PremiumEnv for PremiumTdEnv<'_> {
    fn get_option_integer(&self, name: &str) -> i64 {
        self.options
            .as_ref()
            .map_or(0, |options| options.get_option_integer(name, 0))
    }

    fn get_option_string(&self, name: &str, default_value: &str) -> String {
        self.options.as_ref().map_or_else(
            || default_value.to_string(),
            |options| options.get_option_string(name, default_value),
        )
    }

    fn send_update(&mut self, _update: td_api::Update) {}
}

/// The quick-reply env over the `Td` fields.
pub(crate) struct QuickReplyEnv<'a> {
    pub(crate) global: &'a mut Global,
}

impl QuickReplyManagerEnv for QuickReplyEnv<'_> {
    fn close_flag(&self) -> bool {
        self.global.close_flag()
    }

    fn send_update(&mut self, _update: td_api::Update) {}
}

/// The boost-plane env over the `Td` fields.
pub(crate) struct BoostEnv<'a> {
    pub(crate) global: &'a mut Global,
    pub(crate) options: Option<&'a mut OptionManager>,
    pub(crate) chat_manager: &'a mut crate::chat_manager::ChatManager,
}

impl BoostManagerEnv for BoostEnv<'_> {
    fn close_flag(&self) -> bool {
        self.global.close_flag()
    }

    fn get_option_integer(&self, name: &str, default_value: i64) -> i64 {
        self.options.as_ref().map_or(default_value, |options| {
            options.get_option_integer(name, default_value)
        })
    }

    fn send_update(&mut self, _update: td_api::Update) {}

    fn check_dialog_access(
        &mut self,
        dialog_id: crate::dialog_id::DialogId,
    ) -> std::result::Result<(), tdutils::status::Status> {
        if dialog_id.is_valid() {
            Ok(())
        } else {
            Err(tdutils::status::Status::error(400, "Chat not found"))
        }
    }

    fn get_channel_first_username(&mut self, channel_id: crate::chat_manager::ChannelId) -> String {
        // `ChatManager::get_channel_first_username` — the M.5 surface; the
        // seam answers the empty username.
        let _ = (
            self.chat_manager as &mut crate::chat_manager::ChatManager,
            channel_id,
        );
        String::new()
    }
}

/// The [`crate::autosave_manager::AutosaveManagerEnv`] over the `Td`
/// fields — built per call with the field-split borrows (the
/// `NotificationEnv` convention), never held across calls. The sqlite pmc
/// surface routes to `TdDb::common_kv` (the C++ `get_sqlite_pmc` home);
/// the calls are gated on `use_message_database` by the manager, matching
/// the C++ branch that guards them.
pub(crate) struct AutosaveEnv<'a> {
    pub(crate) global: &'a mut Global,
    pub(crate) updates: &'a mut Vec<td_api::Update>,
}

impl crate::autosave_manager::AutosaveManagerEnv for AutosaveEnv<'_> {
    fn close_flag(&self) -> bool {
        self.global.close_flag()
    }

    fn use_message_database(&self) -> bool {
        self.global.use_message_database()
    }

    fn sqlite_pmc_get_raw(&mut self, key: &[u8]) -> Vec<u8> {
        self.global.td_db().common_kv().get(key)
    }

    fn sqlite_pmc_set_raw(&mut self, key: &[u8], value: &[u8]) {
        self.global.td_db().common_kv().set(key, value);
    }

    fn binlog_pmc_erase(&mut self, key: &str) {
        self.global.binlog_pmc().erase(key);
    }

    fn send_update(&mut self, update: td_api::Update) {
        self.updates.push(update);
    }

    /// the simplified dialog seam until the M.5/M.7 dialog integration
    /// (the BoostEnv convention)
    fn check_dialog_access(
        &mut self,
        dialog_id: crate::dialog_id::DialogId,
    ) -> Result<(), tdutils::status::Status> {
        if dialog_id.is_valid() {
            Ok(())
        } else {
            Err(tdutils::status::Status::error(400, "Chat not found"))
        }
    }

    fn force_create_dialog(&mut self, _dialog_id: crate::dialog_id::DialogId) {
        // `dialog_manager::force_create_dialog` — the M.6/M.7 cache seam;
        // every dialog id is resolvable in the collapsed cache
    }
}

/// The [`crate::attach_menu_manager::AttachMenuManagerEnv`] over the `Td`
/// fields — built per call with the field-split borrows (the
/// `NotificationEnv` convention), never held across calls; the file
/// manager rides its `Arc` clone so the disjoint borrows hold.
pub(crate) struct AttachMenuEnv<'a> {
    pub(crate) global: &'a mut Global,
    pub(crate) options: Option<&'a mut OptionManager>,
    pub(crate) auth_is_authorized: bool,
    pub(crate) auth_is_bot: bool,
    pub(crate) user_manager: &'a mut UserManager,
    pub(crate) file_manager: std::sync::Arc<parking_lot::Mutex<crate::files::FileManager>>,
    pub(crate) file_reference_manager: &'a mut crate::files::FileReferenceManager,
    pub(crate) updates: &'a mut Vec<td_api::Update>,
}

impl crate::attach_menu_manager::AttachMenuManagerEnv for AttachMenuEnv<'_> {
    fn close_flag(&self) -> bool {
        self.global.close_flag()
    }

    fn is_authorized(&self) -> bool {
        self.auth_is_authorized
    }

    fn is_bot(&self) -> bool {
        self.auth_is_bot
    }

    fn use_chat_info_database(&self) -> bool {
        self.global.use_chat_info_database()
    }

    fn binlog_pmc_get_raw(&mut self, key: &[u8]) -> Vec<u8> {
        self.global.binlog_pmc().get_raw(key)
    }

    fn binlog_pmc_set_raw(&mut self, key: &[u8], value: &[u8]) {
        self.global.binlog_pmc().set_raw(key, value);
    }

    fn binlog_pmc_erase(&mut self, key: &str) {
        self.global.binlog_pmc().erase(key);
    }

    fn send_update(&mut self, update: td_api::Update) {
        self.updates.push(update);
    }

    fn get_input_user(
        &mut self,
        user_id: UserId,
    ) -> Result<telegram_api::InputUser, tdutils::status::Status> {
        let user_env = UserEnv {
            global: self.global,
            options: self
                .options
                .as_mut()
                .expect("attach-menu requests run with the initialized option manager"),
            auth_is_bot: self.auth_is_bot,
        };
        self.user_manager.get_input_user(&user_env, user_id)
    }

    fn get_bot_data(
        &mut self,
        user_id: UserId,
    ) -> Result<crate::user_manager::BotData, tdutils::status::Status> {
        self.user_manager.get_bot_data(user_id)
    }

    fn have_accessible_user(&self, user_id: UserId) -> bool {
        self.user_manager.have_accessible_user(user_id)
    }

    fn get_user_id_object(&mut self, user_id: UserId) -> i64 {
        let mut user_env = UserEnv {
            global: self.global,
            options: self
                .options
                .as_mut()
                .expect("attach-menu requests run with the initialized option manager"),
            auth_is_bot: self.auth_is_bot,
        };
        self.user_manager.get_user_id_object(
            &mut user_env,
            user_id,
            "get_attachment_menu_bot_object",
        )
    }

    fn on_get_users(&mut self, users: Vec<telegram_api::User>, source: &str) {
        let mut user_env = UserEnv {
            global: self.global,
            options: self
                .options
                .as_mut()
                .expect("attach-menu requests run with the initialized option manager"),
            auth_is_bot: self.auth_is_bot,
        };
        self.user_manager.on_get_users(&mut user_env, users, source)
    }

    fn get_attach_menu_bot_file_source_id(&mut self, user_id: UserId) -> FileSourceId {
        // `FileReferenceManager::create_attach_menu_bot_file_source`
        // (FileReferenceManager.cpp:282-286)
        self.file_reference_manager
            .intern(crate::files::FileSource::AttachMenuBot(user_id))
    }

    fn on_get_icon_document(
        &mut self,
        document: &telegram_api::document,
        expected_sticker: bool,
    ) -> FileId {
        // the narrow `DocumentsManager::on_get_document` collapse: the
        // General-vs-Sticker type derivation reads the document
        // attributes; the remote location registers into the M.12 file
        // plane with the C++ FromServer source
        let has_sticker = document.attributes.iter().any(|attribute| {
            matches!(
                attribute,
                telegram_api::DocumentAttribute::documentAttributeSticker(_)
            )
        });
        if has_sticker != expected_sticker {
            return FileId::new(0, 0);
        }
        let file_type = if has_sticker {
            FileType::Sticker
        } else {
            FileType::Document
        };
        let dc_id = if DcId::is_valid_raw(document.dc_id) {
            DcId::internal(document.dc_id)
        } else {
            DcId::invalid()
        };
        let location = crate::files::FullRemoteFileLocation::from_document(
            file_type,
            document.id,
            document.access_hash,
            dc_id,
            String::from_utf8_lossy(&document.file_reference).into_owned(),
        );
        let remote_name = document
            .attributes
            .iter()
            .find_map(|attribute| match attribute {
                telegram_api::DocumentAttribute::documentAttributeFilename(name) => {
                    Some(name.file_name.clone())
                }
                _ => None,
            })
            .unwrap_or_else(|| document.mime_type.clone());
        let mut file_manager = self.file_manager.lock();
        file_manager.register_remote(
            location,
            crate::files::FileLocationSource::FromServer,
            crate::message_id::DialogId::new(0),
            document.size,
            document.size,
            remote_name,
        )
    }

    fn add_file_source(&mut self, file_id: FileId, source_id: FileSourceId) {
        // the C++ `FileManager::add_file_source` hop routes into the
        // FileReferenceManager, which owns the source registry
        self.file_reference_manager
            .add_file_source(file_id, source_id);
    }

    fn get_file_object(&mut self, file_id: FileId) -> td_api::File {
        self.file_manager.lock().get_file_object(file_id)
    }

    fn store_file_id(&mut self, file_id: FileId, out: &mut Vec<u8>) {
        self.file_manager.lock().store_file(file_id, out);
    }

    fn parse_file_id(
        &mut self,
        parser: &mut crate::store_parse::Parser,
    ) -> Result<FileId, &'static str> {
        self.file_manager.lock().parse_file(parser)
    }
}

/// The [`crate::theme_manager::ThemeManagerEnv`] over the `Td` fields —
/// built per call with the field-split borrows (the `NotificationEnv`
/// convention), never held across calls; the file manager rides its `Arc`
/// clone so the disjoint borrows hold.
pub(crate) struct ThemeEnv<'a> {
    pub(crate) global: &'a mut Global,
    pub(crate) options: Option<&'a mut OptionManager>,
    pub(crate) auth_is_authorized: bool,
    pub(crate) auth_is_bot: bool,
    pub(crate) user_manager: &'a mut UserManager,
    pub(crate) chat_manager: &'a mut crate::chat_manager::ChatManager,
    pub(crate) file_manager: std::sync::Arc<parking_lot::Mutex<crate::files::FileManager>>,
    pub(crate) updates: &'a mut Vec<td_api::Update>,
}

impl crate::theme_manager::ThemeManagerEnv for ThemeEnv<'_> {
    fn is_authorized(&self) -> bool {
        self.auth_is_authorized
    }

    fn is_bot(&self) -> bool {
        self.auth_is_bot
    }

    fn binlog_pmc_get_raw(&mut self, key: &[u8]) -> Vec<u8> {
        self.global.binlog_pmc().get_raw(key)
    }

    fn binlog_pmc_set_raw(&mut self, key: &[u8], value: &[u8]) {
        self.global.binlog_pmc().set_raw(key, value);
    }

    fn get_option_integer(&mut self, name: &str) -> i64 {
        self.options
            .as_mut()
            .map(|options| options.get_option_integer(name, 0))
            .unwrap_or(0)
    }

    fn send_update(&mut self, update: td_api::Update) {
        self.updates.push(update);
    }

    fn on_get_users(&mut self, users: Vec<telegram_api::User>, source: &str) {
        let mut user_env = UserEnv {
            global: self.global,
            options: self
                .options
                .as_mut()
                .expect("theme queries run with the initialized option manager"),
            auth_is_bot: self.auth_is_bot,
        };
        self.user_manager.on_get_users(&mut user_env, users, source);
    }

    fn on_get_chats(&mut self, chats: Vec<telegram_api::Chat>, source: &str) {
        let mut user_env = UserEnv {
            global: self.global,
            options: self
                .options
                .as_mut()
                .expect("theme queries run with the initialized option manager"),
            auth_is_bot: self.auth_is_bot,
        };
        self.chat_manager
            .on_get_chats(self.user_manager, &mut user_env, chats, source);
    }

    fn on_get_theme_background(
        &mut self,
        wallpaper: Option<telegram_api::WallPaper>,
        is_no_file_pattern: bool,
    ) -> Option<(
        crate::theme_manager::BackgroundId,
        crate::theme_manager::BackgroundType,
    )> {
        use crate::chat_manager::DcId;
        use crate::files::{FileType, FullRemoteFileLocation};

        let wallpaper = wallpaper?;
        match wallpaper {
            // the wallPaperNoFile half (BackgroundManager.cpp:1287-1310):
            // the settings are required, the id must be valid and remote;
            // the local-id counter is [blocked: M.16 Backgrounds]
            telegram_api::WallPaper::wallPaperNoFile(wallpaper) => {
                let settings = wallpaper.settings?;
                let background_id = crate::theme_manager::BackgroundId::new(wallpaper.id);
                if !background_id.is_valid() || background_id.is_local() {
                    tdutils::log_error!("Receive an invalid no-file wallpaper id");
                    return None;
                }
                let background_type = crate::theme_manager::BackgroundType::from_settings(
                    true,
                    is_no_file_pattern,
                    Some(&settings),
                );
                Some((background_id, background_type))
            }
            // the wallPaper half (BackgroundManager.cpp:1312-1360): the id
            // checks are inline, the document registers into the M.12 file
            // plane with the C++ FromServer source (the
            // DocumentsManager::on_get_document General/wallpaper collapse)
            telegram_api::WallPaper::wallPaper(wallpaper) => {
                let background_id = crate::theme_manager::BackgroundId::new(wallpaper.id);
                if !background_id.is_valid() || background_id.is_local() {
                    tdutils::log_error!("Receive an invalid wallpaper id");
                    return None;
                }
                let is_pattern = wallpaper.pattern;
                let Some(document) = wallpaper.document.as_ref() else {
                    tdutils::log_error!("Receive a wallpaper without a document");
                    return None;
                };
                let telegram_api::Document::document(document) = document else {
                    tdutils::log_error!("Receive a wallpaper with documentEmpty");
                    return None;
                };
                let dc_id = if DcId::is_valid_raw(document.dc_id) {
                    DcId::internal(document.dc_id)
                } else {
                    DcId::invalid()
                };
                let location = FullRemoteFileLocation::from_document(
                    FileType::Wallpaper,
                    document.id,
                    document.access_hash,
                    dc_id,
                    String::from_utf8_lossy(&document.file_reference).into_owned(),
                );
                let remote_name = document
                    .attributes
                    .iter()
                    .find_map(|attribute| match attribute {
                        telegram_api::DocumentAttribute::documentAttributeFilename(name) => {
                            Some(name.file_name.clone())
                        }
                        _ => None,
                    })
                    .unwrap_or_else(|| document.mime_type.clone());
                let mut file_manager = self.file_manager.lock();
                let file_id = file_manager.register_remote(
                    location,
                    crate::files::FileLocationSource::FromServer,
                    crate::message_id::DialogId::new(0),
                    document.size,
                    document.size,
                    remote_name,
                );
                if !file_id.is_valid() {
                    tdutils::log_error!("Receive a wallpaper with an invalid document");
                    return None;
                }
                let background_type = crate::theme_manager::BackgroundType::from_settings(
                    false,
                    is_pattern,
                    wallpaper.settings.as_ref(),
                );
                Some((background_id, background_type))
            }
        }
    }

    fn get_theme_background_object(
        &mut self,
        info: &crate::theme_manager::BackgroundInfo,
    ) -> Option<td_api::Background> {
        // the BackgroundManager object cache ([blocked: M.16 Backgrounds])
        let _ = info;
        None
    }

    fn store_background_row(
        &mut self,
        background_id: crate::theme_manager::BackgroundId,
        out: &mut Vec<u8>,
    ) -> bool {
        // the full Background row store ([blocked: M.16 Backgrounds]) —
        // the Td-side cache cannot answer yet, so a wallpaper-bearing
        // theme stores background-less
        let _ = (background_id, out);
        false
    }

    fn parse_background_row(
        &mut self,
        parser: &mut crate::store_parse::Parser,
    ) -> Result<crate::theme_manager::BackgroundId, &'static str> {
        // the symmetric half of the store seam; with the store returning
        // false the has_background flag keeps the row out of the stream
        let _ = parser;
        Result::Ok(crate::theme_manager::BackgroundId::new(0))
    }

    fn star_gift_object_for_theme(
        &mut self,
        gift: telegram_api::StarGift,
    ) -> Option<td_api::UpgradedGift> {
        // the StarGift value plane ([blocked: M.42 StarGift])
        let _ = gift;
        tdutils::log_info!("StarGift for the chat theme — [blocked: M.42 StarGift]");
        None
    }

    fn send_get_chat_themes_query(&mut self, hash: i64) {
        tdutils::log_info!("Send account.getChatThemes(hash = {hash}) — [blocked: net]");
    }

    fn send_get_peer_colors_query(&mut self, hash: i32) {
        tdutils::log_info!("Send help.getPeerColors(hash = {hash}) — [blocked: net]");
    }

    fn send_get_peer_profile_colors_query(&mut self, hash: i32) {
        tdutils::log_info!("Send help.getPeerProfileColors(hash = {hash}) — [blocked: net]");
    }

    fn send_get_unique_gift_chat_themes_query(&mut self, offset: &str, limit: i32) {
        tdutils::log_info!(
            "Send account.getUniqueGiftChatThemes(offset = {offset}, limit = {limit}) — [blocked: net]"
        );
    }
}

/// The `G()`/`td_` slice [`crate::star_gift_manager::StarGiftManager`]
/// reads through (the C++ manager reads the same surfaces off its
/// `Td *td_` directly; the collapsed manager runs inside a `&mut Td`
/// borrow, so the env carries the disjoint field borrows). The
/// StickersManager/StarManager/MessagesManager/PasswordManager/
/// BusinessConnectionManager halves keep their trait defaults — those
/// managers are their own tracks (see the star_gift_manager module
/// header); the user/dialog halves below delegate to the real read
/// surface with the M.6.15a chat stubs.
pub(crate) struct StarGiftTdEnv<'a> {
    pub(crate) global: &'a mut Global,
    /// `None` once the option manager is torn down; the getters answer
    /// their defaults like the other envs.
    pub(crate) options: Option<&'a mut OptionManager>,
    pub(crate) auth_is_authorized: bool,
    pub(crate) auth_is_bot: bool,
    pub(crate) user_manager: &'a mut UserManager,
    pub(crate) chat_manager: &'a mut crate::chat_manager::ChatManager,
    /// `dialog_manager_->get_my_dialog_id()` (the M.6.15a free-function
    /// answer, snapshotted at construction).
    pub(crate) my_dialog_id: crate::dialog_id::DialogId,
    pub(crate) updates: &'a mut Vec<td_api::Update>,
}

impl crate::star_gift_manager::StarGiftEnv for StarGiftTdEnv<'_> {
    fn is_authorized(&self) -> bool {
        self.auth_is_authorized
    }

    fn is_bot(&self) -> bool {
        self.auth_is_bot
    }

    fn unix_time(&self) -> i32 {
        self.global.unix_time()
    }

    fn get_option_integer(&mut self, name: &str) -> i64 {
        self.options
            .as_ref()
            .map_or(0, |options| options.get_option_integer(name, 0))
    }

    fn get_option_boolean(&mut self, name: &str) -> bool {
        self.options
            .as_ref()
            .is_some_and(|options| options.get_option_boolean(name, false))
    }

    fn get_option_string(&mut self, name: &str, default_value: &str) -> String {
        self.options.as_ref().map_or_else(
            || default_value.to_owned(),
            |options| options.get_option_string(name, default_value),
        )
    }

    fn send_update(&mut self, update: td_api::Update) {
        self.updates.push(update);
    }

    fn on_get_users(&mut self, users: Vec<telegram_api::User>, source: &str) {
        let crate::td::StarGiftTdEnv {
            global,
            options,
            auth_is_bot,
            user_manager,
            ..
        } = self;
        let Some(options) = options else {
            return;
        };
        let mut env = UserEnv {
            global,
            options,
            auth_is_bot: *auth_is_bot,
        };
        user_manager.on_get_users(&mut env, users, source);
    }

    fn on_get_chats(&mut self, chats: Vec<telegram_api::Chat>, source: &str) {
        let crate::td::StarGiftTdEnv {
            global,
            options,
            auth_is_bot,
            user_manager,
            chat_manager,
            ..
        } = self;
        let Some(options) = options else {
            return;
        };
        let mut env = UserEnv {
            global,
            options,
            auth_is_bot: *auth_is_bot,
        };
        chat_manager.on_get_chats(user_manager, &mut env, chats, source);
    }

    fn have_user_force(&mut self, user_id: UserId, source: &str) -> bool {
        let Some(options) = self.options.as_mut() else {
            return false;
        };
        let mut env = UserEnv {
            global: &mut *self.global,
            options,
            auth_is_bot: self.auth_is_bot,
        };
        self.user_manager.have_user_force(&mut env, user_id, source)
    }

    fn get_user_id_object(&mut self, user_id: UserId, source: &str) -> i64 {
        let Some(options) = self.options.as_mut() else {
            return user_id.get();
        };
        let mut env = UserEnv {
            global: &mut *self.global,
            options,
            auth_is_bot: self.auth_is_bot,
        };
        self.user_manager
            .get_user_id_object(&mut env, user_id, source)
    }

    fn get_user_ids_object(&mut self, user_ids: &[UserId], _source: &str) -> Vec<i64> {
        user_ids.iter().map(|user_id| user_id.get()).collect()
    }

    fn get_my_dialog_id(&self) -> crate::dialog_id::DialogId {
        self.my_dialog_id
    }

    fn check_dialog_access(
        &mut self,
        dialog_id: crate::dialog_id::DialogId,
        _source: &str,
    ) -> std::result::Result<(), tdutils::status::Status> {
        use crate::dialog_id::DialogType;
        // the M.6.15a halves: the chat/channel/`have_dialog` lookups are
        // the stub inventory, so only the user/secret-chat info can pass
        let have = match dialog_id.get_type() {
            DialogType::User => self
                .user_manager
                .have_accessible_user(dialog_id.get_user_id()),
            DialogType::SecretChat => self
                .user_manager
                .have_secret_chat(dialog_id.get_secret_chat_id()),
            _ => false,
        };
        if !have {
            if !dialog_id.is_valid() {
                return std::result::Result::Err(tdutils::status::Status::error(
                    400,
                    "Invalid chat identifier specified",
                ));
            }
            return std::result::Result::Err(tdutils::status::Status::error(400, "Chat not found"));
        }
        // check_dialog_access_in_memory: the user half through the real
        // input-peer check, the rest through the stub `false`
        let have_input_peer = match dialog_id.get_type() {
            DialogType::User => {
                let global = &mut *self.global;
                let Some(options) = self.options.as_mut() else {
                    return std::result::Result::Err(tdutils::status::Status::error(
                        400,
                        "Can't access the chat",
                    ));
                };
                let env = UserEnv {
                    global,
                    options,
                    auth_is_bot: self.auth_is_bot,
                };
                self.user_manager.have_input_peer_user(
                    &env,
                    dialog_id.get_user_id(),
                    crate::user_manager::AccessRights::Read,
                )
            }
            _ => false,
        };
        if !have_input_peer {
            return std::result::Result::Err(tdutils::status::Status::error(
                400,
                "Can't access the chat",
            ));
        }
        std::result::Result::Ok(())
    }

    fn get_input_peer(
        &mut self,
        dialog_id: crate::dialog_id::DialogId,
    ) -> Option<telegram_api::InputPeer> {
        use crate::dialog_id::DialogType;
        match dialog_id.get_type() {
            DialogType::User => {
                let options = self.options.as_mut()?;
                let env = UserEnv {
                    global: &mut *self.global,
                    options,
                    auth_is_bot: self.auth_is_bot,
                };
                self.user_manager.get_input_peer_user(
                    &env,
                    dialog_id.get_user_id(),
                    crate::user_manager::AccessRights::Read,
                )
            }
            // the chat/secret-chat halves are the M.6.15a stubs
            _ => None,
        }
    }

    fn have_dialog_force(&mut self, dialog_id: crate::dialog_id::DialogId, _source: &str) -> bool {
        use crate::dialog_id::DialogType;
        match dialog_id.get_type() {
            DialogType::User => self
                .user_manager
                .have_accessible_user(dialog_id.get_user_id()),
            DialogType::SecretChat => self
                .user_manager
                .have_secret_chat(dialog_id.get_secret_chat_id()),
            _ => false,
        }
    }

    fn get_chat_id_object(&mut self, dialog_id: crate::dialog_id::DialogId, _source: &str) -> i64 {
        // the M.6.16 stub: the MessagesManager lookup answers the raw id
        dialog_id.get()
    }

    // the user/chat gift-count updates and the channel-status read ride
    // the UserManager-full/ChatManager planes (their own tracks); the
    // force-create is the M.6 no-op stub

    fn on_update_my_gift_count(&mut self, _delta: i32) {}

    fn on_update_user_gift_count(&mut self, _user_id: UserId, _count: i32) {}

    fn force_create_dialog(&mut self, _dialog_id: crate::dialog_id::DialogId, _source: &str) {}

    fn is_broadcast_channel(&mut self, _dialog_id: crate::dialog_id::DialogId) -> bool {
        false
    }

    fn can_channel_post_messages(&mut self, _channel_id: crate::chat_manager::ChannelId) -> bool {
        false
    }

    fn on_update_channel_gift_count(
        &mut self,
        _channel_id: crate::chat_manager::ChannelId,
        _count: i32,
        _is_relax: bool,
    ) {
    }
}

/// The [`crate::payments::PaymentsEnv`] over the `Td` fields — built per
/// request with the field-split borrows (the `NotificationEnv`
/// convention), never held across calls. The UserManager reads are real;
/// the DialogManager halves mirror the M.6 stub inventory (the chat/
/// channel-backed reads stay the unloaded results until M.10), and the
/// net/MessagesManager/StarManager/Files seams keep their trait defaults.
pub(crate) struct PaymentsEnv<'a> {
    pub(crate) global: &'a mut Global,
    pub(crate) options: Option<&'a mut OptionManager>,
    pub(crate) auth_is_bot: bool,
    /// `dialog_manager::get_my_dialog_id(td)`, computed before the field
    /// split (the free function needs the whole `Td`).
    pub(crate) my_dialog_id: crate::dialog_id::DialogId,
    pub(crate) user_manager: &'a mut UserManager,
    pub(crate) chat_manager: &'a mut crate::chat_manager::ChatManager,
}

impl crate::payments::PaymentsEnv for PaymentsEnv<'_> {
    fn get_unix_time(&self) -> i32 {
        self.global.unix_time()
    }

    fn get_option_integer(&mut self, name: &str) -> i64 {
        self.options
            .as_mut()
            .map(|options| options.get_option_integer(name, 0))
            .unwrap_or(0)
    }

    fn binlog_pmc_get_raw(&mut self, key: &[u8]) -> Vec<u8> {
        self.global.binlog_pmc().get_raw(key)
    }

    fn get_input_user(
        &mut self,
        user_id: UserId,
    ) -> tdutils::status::Result<telegram_api::InputUser> {
        let (env, user_manager) = self.split_user_env();
        match user_manager.get_input_user(&env, user_id) {
            Ok(input_user) => tdutils::status::Result::Ok(input_user),
            Err(status) => tdutils::status::Result::Err(status),
        }
    }

    fn get_input_users(
        &mut self,
        user_ids: &[UserId],
    ) -> tdutils::status::Result<Vec<telegram_api::InputUser>> {
        let (env, user_manager) = self.split_user_env();
        match user_manager.get_input_users(&env, user_ids) {
            Ok(input_users) => tdutils::status::Result::Ok(input_users),
            Err(status) => tdutils::status::Result::Err(status),
        }
    }

    fn on_get_users(&mut self, users: Vec<telegram_api::User>, source: &str) {
        let (mut env, user_manager) = self.split_user_env();
        user_manager.on_get_users(&mut env, users, source);
    }

    fn get_user_id_object(&mut self, user_id: UserId, source: &str) -> i64 {
        let (mut env, user_manager) = self.split_user_env();
        user_manager.get_user_id_object(&mut env, user_id, source)
    }

    fn have_dialog_force(&mut self, dialog_id: crate::dialog_id::DialogId, _source: &str) -> bool {
        // `DialogManager::have_dialog_force` → the M.6 stub inventory
        // (dialog_manager::have_dialog_info): the chat/channel halves are
        // the unloaded results until M.10
        use crate::dialog_id::DialogType;
        match dialog_id.get_type() {
            DialogType::User => self
                .user_manager
                .have_accessible_user(dialog_id.get_user_id()),
            DialogType::SecretChat => self
                .user_manager
                .have_secret_chat(dialog_id.get_secret_chat_id()),
            DialogType::Chat | DialogType::Channel | DialogType::None => false,
        }
    }

    fn get_channel_status(
        &mut self,
        channel_id: crate::chat_manager::ChannelId,
    ) -> crate::chat_manager::DialogParticipantStatus {
        self.chat_manager.get_channel_status(channel_id)
    }

    fn is_broadcast_channel(&mut self, channel_id: crate::chat_manager::ChannelId) -> bool {
        self.chat_manager.is_broadcast_channel(channel_id)
    }

    fn get_input_peer(
        &mut self,
        dialog_id: crate::dialog_id::DialogId,
        access_rights: crate::user_manager::AccessRights,
    ) -> Option<telegram_api::InputPeer> {
        // `DialogManager::get_input_peer` over the split fields (the M.6
        // function re-derives the same `UserEnv`; its chat/channel halves
        // are the stub inventory's `None` until M.10)
        use crate::dialog_id::DialogType;
        match dialog_id.get_type() {
            DialogType::User => {
                let (env, user_manager) = self.split_user_env();
                user_manager.get_input_peer_user(&env, dialog_id.get_user_id(), access_rights)
            }
            DialogType::Chat | DialogType::Channel | DialogType::SecretChat => None,
            DialogType::None => Some(telegram_api::InputPeer::inputPeerEmpty(
                telegram_api::inputPeerEmpty {},
            )),
        }
    }

    fn check_dialog_access(
        &mut self,
        dialog_id: crate::dialog_id::DialogId,
        source: &str,
    ) -> tdutils::status::Result<()> {
        // `DialogManager::check_dialog_access(dialog_id, false, Read,
        // source)` over the stub inventory (the in-memory halves the M.6
        // inventory stubs to `true` for the accessible users)
        if !self.have_dialog_force(dialog_id, source) {
            if !dialog_id.is_valid() {
                return tdutils::status::Result::from_status(tdutils::status::Status::error(
                    400,
                    "Invalid chat identifier specified",
                ));
            }
            return tdutils::status::Result::from_status(tdutils::status::Status::error(
                400,
                "Chat not found",
            ));
        }
        tdutils::status::Result::Ok(())
    }

    fn get_my_dialog_id(&self) -> crate::dialog_id::DialogId {
        self.my_dialog_id
    }
}

impl PaymentsEnv<'_> {
    /// Field-split the env into the `UserEnv` plus the manager reference
    /// (the `user_env!` shape; a `&mut self` helper cannot return both,
    /// hence the manual destructuring at every call).
    fn split_user_env(&mut self) -> (UserEnv<'_>, &mut UserManager) {
        let Self {
            global,
            options,
            auth_is_bot,
            my_dialog_id: _,
            user_manager,
            chat_manager: _,
        } = self;
        (
            UserEnv {
                global,
                options: options
                    .as_mut()
                    .expect("payment requests run with the initialized option manager"),
                auth_is_bot: *auth_is_bot,
            },
            user_manager,
        )
    }
}

/// The [`crate::secure_value::SecureValueEnv`] +
/// [`crate::secure_manager::SecureManagerEnv`] over the `Td` fields — built
/// per call with the field-split borrows (the `NotificationEnv`
/// convention), never held across calls; the M.12 file plane rides its
/// `Arc` clone so the disjoint borrows hold.
pub(crate) struct SecureEnv<'a> {
    pub(crate) global: &'a mut Global,
    /// the user-only passport requests run after `init`
    pub(crate) options: &'a mut OptionManager,
    pub(crate) auth_is_bot: bool,
    pub(crate) user_manager: &'a mut UserManager,
    pub(crate) file_manager: Arc<parking_lot::Mutex<FileManager>>,
    pub(crate) file_load_manager: Arc<parking_lot::Mutex<FileLoadManager>>,
    /// the snapshot of `Td::close_flag`
    pub(crate) close_flag: bool,
}

impl crate::secure_value::SecureValueEnv for SecureEnv<'_> {
    fn close_flag(&self) -> bool {
        self.close_flag
    }

    fn unix_time(&mut self) -> i32 {
        self.global.unix_time()
    }

    fn file_register_secure_encrypted(
        &mut self,
        id: i64,
        access_hash: i64,
        dc_id: DcId,
        size: i64,
        remote_name: String,
    ) -> FileId {
        self.file_manager.lock().register_remote(
            crate::files::FullRemoteFileLocation::from_document(
                FileType::SecureEncrypted,
                id,
                access_hash,
                dc_id,
                String::new(),
            ),
            crate::files::FileLocationSource::FromServer,
            DialogId::default(),
            size,
            0,
            remote_name,
        )
    }

    fn file_register_secure_decrypted(
        &mut self,
        id: i64,
        access_hash: i64,
        dc_id: DcId,
        expected_size: i64,
        suggested_path: String,
    ) -> FileId {
        self.file_manager.lock().register_remote(
            crate::files::FullRemoteFileLocation::from_document(
                FileType::SecureDecrypted,
                id,
                access_hash,
                dc_id,
                String::new(),
            ),
            crate::files::FileLocationSource::FromServer,
            DialogId::default(),
            0,
            expected_size,
            suggested_path,
        )
    }

    fn file_view(&mut self, file_id: FileId) -> Option<crate::secure_value::SecureFileView> {
        let file_manager = self.file_manager.lock();
        let view = file_manager.get_file_view(file_id)?;
        let full_remote = view.get_full_remote_location();
        Some(crate::secure_value::SecureFileView {
            file_type: view.get_type(),
            main_file_id: view.get_main_file_id(),
            is_encrypted_secure: view.is_encrypted_secure(),
            full_remote: full_remote.map(|location| crate::secure_value::SecureFullRemote {
                id: location.get_id(),
                access_hash: location.get_access_hash(),
                dc_id: location.get_dc_id(),
            }),
            full_remote_is_web: full_remote.is_some_and(|location| location.is_web()),
            encryption_key: view.encryption_key().clone(),
            expected_size: view.expected_size(false),
            suggested_path: view.suggested_path(),
            size: view.size(),
        })
    }

    fn file_set_encryption_key(&mut self, file_id: FileId, key: crate::files::FileEncryptionKey) {
        self.file_manager.lock().set_encryption_key(file_id, key);
    }

    fn file_object(&mut self, file_id: FileId) -> td_api::File {
        self.file_manager.lock().get_file_object(file_id)
    }

    fn file_get_input_file_id(
        &mut self,
        file: Option<&td_api::InputFile>,
    ) -> tdutils::status::Result<FileId> {
        match self.file_manager.lock().get_input_file_id(
            FileType::SecureEncrypted,
            file,
            DialogId::default(),
            false,
            false,
            false,
            true,
            false,
            false,
        ) {
            Ok(file_id) => tdutils::status::Result::Ok(file_id),
            Err(status) => tdutils::status::Result::Err(status),
        }
    }

    fn file_merge(
        &mut self,
        to_file_id: FileId,
        from_file_id: FileId,
    ) -> tdutils::status::Result<()> {
        match self.file_manager.lock().merge(to_file_id, from_file_id) {
            Ok(()) => tdutils::status::Result::Ok(()),
            Err(status) => tdutils::status::Result::Err(status),
        }
    }

    fn file_copy_file_id(&mut self, file_id: FileId) -> FileId {
        self.file_manager.lock().copy_file_id(
            file_id,
            FileType::SecureEncrypted,
            DialogId::default(),
            "SetSecureValue",
        )
    }

    fn file_cancel_upload(&mut self, file_upload_id: crate::files::FileUploadId) {
        let mut file_load_manager = self.file_load_manager.lock();
        self.file_manager
            .lock()
            .cancel_upload(file_upload_id, &mut file_load_manager);
    }

    fn file_get_internal_upload_id(&mut self) -> i64 {
        self.file_manager.lock().get_internal_upload_id()
    }
}

impl crate::secure_manager::SecureManagerEnv for SecureEnv<'_> {
    fn on_get_users(&mut self, users: Vec<telegram_api::User>, source: &str) {
        let mut user_env = UserEnv {
            global: self.global,
            options: self.options,
            auth_is_bot: self.auth_is_bot,
        };
        self.user_manager.on_get_users(&mut user_env, users, source);
    }

    fn get_input_user(
        &mut self,
        user_id: UserId,
    ) -> tdutils::status::Result<telegram_api::InputUser> {
        let user_env = UserEnv {
            global: self.global,
            options: self.options,
            auth_is_bot: self.auth_is_bot,
        };
        match self.user_manager.get_input_user(&user_env, user_id) {
            Ok(input_user) => tdutils::status::Result::Ok(input_user),
            Err(status) => tdutils::status::Result::Err(status),
        }
    }
}

/// The [`crate::bot_info_manager::BotInfoManagerEnv`] over the `Td` fields
/// — built per call with the field-split borrows (the `ThemeEnv`
/// convention, M.41), never held across calls.
pub(crate) struct BotInfoEnv<'a> {
    pub(crate) global: &'a mut Global,
    pub(crate) options: Option<&'a mut OptionManager>,
    pub(crate) auth_is_authorized: bool,
    pub(crate) auth_is_bot: bool,
    pub(crate) user_manager: &'a mut UserManager,
}

impl crate::bot_info_manager::BotInfoManagerEnv for BotInfoEnv<'_> {
    fn is_authorized(&self) -> bool {
        self.auth_is_authorized
    }

    fn is_bot(&self) -> bool {
        self.auth_is_bot
    }

    fn is_test_dc(&self) -> bool {
        self.global.is_test_dc()
    }

    fn binlog_pmc_get_raw(&mut self, key: &[u8]) -> Vec<u8> {
        self.global.binlog_pmc().get_raw(key)
    }

    fn binlog_pmc_set_raw(&mut self, key: &[u8], value: &[u8]) {
        self.global.binlog_pmc().set_raw(key, value);
    }

    fn get_option_integer(&mut self, name: &str) -> i64 {
        self.options
            .as_deref()
            .map(|options| options.get_option_integer(name, 0))
            .unwrap_or(0)
    }

    fn send_update(&mut self, update: td_api::Update) {
        // the C++ BotInfoManager never sends updates; the outbox lands in
        // the Td's own send path if a future slice needs it
        tdutils::log_info!("BotInfoManager update send — collapsed into the Td: {update:?}");
    }

    fn get_my_id(&self) -> crate::user_manager::UserId {
        self.user_manager.get_my_id()
    }

    fn get_bot_data(
        &self,
        user_id: crate::user_manager::UserId,
    ) -> std::result::Result<crate::user_manager::BotData, Status> {
        self.user_manager.get_bot_data(user_id)
    }

    fn get_input_user(
        &mut self,
        user_id: crate::user_manager::UserId,
    ) -> std::result::Result<telegram_api::InputUser, Status> {
        let env = UserEnv {
            global: self.global,
            options: self
                .options
                .as_deref_mut()
                .expect("bot requests run with the initialized option manager"),
            auth_is_bot: self.auth_is_bot,
        };
        self.user_manager.get_input_user(&env, user_id)
    }

    fn get_input_users_force(
        &mut self,
        user_ids: &[crate::user_manager::UserId],
    ) -> Vec<telegram_api::InputUser> {
        let env = UserEnv {
            global: self.global,
            options: self
                .options
                .as_deref_mut()
                .expect("bot requests run with the initialized option manager"),
            auth_is_bot: self.auth_is_bot,
        };
        self.user_manager.get_input_users_force(&env, user_ids)
    }

    fn get_user_id_object(&mut self, user_id: crate::user_manager::UserId, source: &str) -> i64 {
        let mut env = UserEnv {
            global: self.global,
            options: self
                .options
                .as_deref_mut()
                .expect("bot requests run with the initialized option manager"),
            auth_is_bot: self.auth_is_bot,
        };
        self.user_manager
            .get_user_id_object(&mut env, user_id, source)
    }

    fn get_user_ids_object(
        &mut self,
        user_ids: &[crate::user_manager::UserId],
        source: &str,
    ) -> Vec<i64> {
        user_ids
            .iter()
            .map(|user_id| self.get_user_id_object(*user_id, source))
            .collect()
    }

    fn get_users_object(
        &mut self,
        total_count: i32,
        user_ids: &[crate::user_manager::UserId],
    ) -> td_api::Users {
        let mut env = UserEnv {
            global: self.global,
            options: self
                .options
                .as_deref_mut()
                .expect("bot requests run with the initialized option manager"),
            auth_is_bot: self.auth_is_bot,
        };
        self.user_manager
            .get_users_object_of(&mut env, total_count, user_ids)
    }

    fn on_get_user(
        &mut self,
        user: telegram_api::User,
        source: &str,
    ) -> crate::user_manager::UserId {
        let mut env = UserEnv {
            global: self.global,
            options: self
                .options
                .as_deref_mut()
                .expect("bot requests run with the initialized option manager"),
            auth_is_bot: self.auth_is_bot,
        };
        self.user_manager.on_get_user(&mut env, user, source)
    }

    fn invalidate_user_full(&mut self, user_id: crate::user_manager::UserId) {
        let mut env = UserEnv {
            global: self.global,
            options: self
                .options
                .as_deref_mut()
                .expect("bot requests run with the initialized option manager"),
            auth_is_bot: self.auth_is_bot,
        };
        self.user_manager.invalidate_user_full(&mut env, user_id);
    }

    fn have_user_force(&mut self, user_id: crate::user_manager::UserId, source: &str) -> bool {
        let mut env = UserEnv {
            global: self.global,
            options: self
                .options
                .as_deref_mut()
                .expect("bot requests run with the initialized option manager"),
            auth_is_bot: self.auth_is_bot,
        };
        self.user_manager.have_user_force(&mut env, user_id, source)
    }

    fn reload_user_full(&mut self, user_id: crate::user_manager::UserId, promise: Promise<()>) {
        // `UserManager::reload_user_full` (the users.getFullUser reload)
        // is [blocked: net]; the forwarded promise dies here like the
        // other net tails — the request stays unanswered until the net
        // step lands
        let _ = (user_id, promise);
        tdutils::log_info!("UserManager::reload_user_full — [blocked: net]");
    }

    fn reload_user(&mut self, user_id: crate::user_manager::UserId, promise: Promise<()>) {
        let _ = (user_id, promise);
        tdutils::log_info!("UserManager::reload_user — [blocked: net]");
    }

    fn on_update_bot_has_preview_medias(
        &mut self,
        user_id: crate::user_manager::UserId,
        has_preview_medias: bool,
    ) {
        let _ = (user_id, has_preview_medias);
        tdutils::log_info!(
            "UserManager::on_update_bot_has_preview_medias — [blocked: the user bot-info step]"
        );
    }

    // The dialog seams: the env-local mirror of the M.6.15a free
    // functions (they take `&mut Td`, which the field-split env cannot
    // reach) — the same UserManager halves, with the Chat/Channel
    // stub inventory.
    fn check_dialog_access(
        &mut self,
        dialog_id: crate::dialog_id::DialogId,
        allow_secret_chats: bool,
        access_rights: crate::user_manager::AccessRights,
        source: &str,
    ) -> tdutils::status::Result<()> {
        use crate::dialog_id::DialogType;
        if !self.have_dialog_force(dialog_id, source) {
            if !dialog_id.is_valid() {
                return tdutils::status::Result::from_status(Status::error(
                    400,
                    "Invalid chat identifier specified",
                ));
            }
            return tdutils::status::Result::from_status(Status::error(400, "Chat not found"));
        }
        if !self.have_input_peer(dialog_id, allow_secret_chats, access_rights) {
            if dialog_id.get_type() == DialogType::SecretChat && !allow_secret_chats {
                return tdutils::status::Result::from_status(Status::error(
                    400,
                    "Not supported in secret chats",
                ));
            }
            if matches!(
                access_rights,
                crate::user_manager::AccessRights::Write | crate::user_manager::AccessRights::Edit
            ) {
                return tdutils::status::Result::from_status(Status::error(
                    400,
                    "Have no write access to the chat",
                ));
            }
            return tdutils::status::Result::from_status(Status::error(
                400,
                "Can't access the chat",
            ));
        }
        tdutils::status::Result::Ok(())
    }

    fn have_input_peer(
        &mut self,
        dialog_id: crate::dialog_id::DialogId,
        allow_secret_chats: bool,
        access_rights: crate::user_manager::AccessRights,
    ) -> bool {
        use crate::dialog_id::DialogType;
        match dialog_id.get_type() {
            DialogType::User => {
                let env = UserEnv {
                    global: self.global,
                    options: self
                        .options
                        .as_deref_mut()
                        .expect("bot requests run with the initialized option manager"),
                    auth_is_bot: self.auth_is_bot,
                };
                self.user_manager
                    .have_input_peer_user(&env, dialog_id.get_user_id(), access_rights)
            }
            // the ChatManager peers are the stub inventory (M.6.15a)
            DialogType::Chat | DialogType::Channel => false,
            DialogType::SecretChat => {
                if !allow_secret_chats {
                    return false;
                }
                self.user_manager
                    .have_input_encrypted_peer(dialog_id.get_secret_chat_id(), access_rights)
            }
            DialogType::None => false,
        }
    }

    fn get_input_peer(
        &mut self,
        dialog_id: crate::dialog_id::DialogId,
        access_rights: crate::user_manager::AccessRights,
    ) -> Option<telegram_api::InputPeer> {
        use crate::dialog_id::DialogType;
        match dialog_id.get_type() {
            DialogType::User => {
                let env = UserEnv {
                    global: self.global,
                    options: self
                        .options
                        .as_deref_mut()
                        .expect("bot requests run with the initialized option manager"),
                    auth_is_bot: self.auth_is_bot,
                };
                self.user_manager
                    .get_input_peer_user(&env, dialog_id.get_user_id(), access_rights)
            }
            // the ChatManager peers are the stub inventory (M.6.15a); the
            // C++ inputPeerSecretChat construction rides
            // ChatManager::get_simple_input_peer (stub inventory too)
            DialogType::Chat | DialogType::Channel | DialogType::SecretChat => None,
            DialogType::None => Some(telegram_api::InputPeer::inputPeerEmpty(
                telegram_api::inputPeerEmpty {},
            )),
        }
    }

    fn is_broadcast_channel(&mut self, dialog_id: crate::dialog_id::DialogId) -> bool {
        // the ChatManager channel read is the stub inventory
        // (dialog_manager::is_broadcast_channel)
        let _ = dialog_id;
        false
    }

    fn have_dialog_force(&mut self, dialog_id: crate::dialog_id::DialogId, source: &str) -> bool {
        use crate::dialog_id::DialogType;
        match dialog_id.get_type() {
            DialogType::User => self.have_user_force(dialog_id.get_user_id(), source),
            // the ChatManager peers are the stub inventory
            DialogType::Chat | DialogType::Channel => false,
            DialogType::SecretChat => self
                .user_manager
                .have_secret_chat(dialog_id.get_secret_chat_id()),
            DialogType::None => false,
        }
    }
}

/// The `G()`/manager slice the four M.46 bot managers read through (the
/// C++ managers read the same surfaces off their `Td *td_` directly):
/// the whole `Td` rides the env, so the dialog/message free functions
/// keep their `&mut Td` signatures (the NotificationEnv field-split is
/// not enough here); the manager state itself is taken out of the `Td`
/// for the call (see the request arms in [`crate::requests`]).
pub(crate) struct BotEnv<'a> {
    pub(crate) td: &'a mut Td,
}

/// The `tdutils::status::Result` → `std::result::Result` rewrap the
/// env traits use (the manager modules return the std form).
fn bot_dialog_result(result: tdutils::status::Result<()>) -> std::result::Result<(), Status> {
    match result {
        tdutils::status::Result::Ok(()) => Ok(()),
        tdutils::status::Result::Err(status) => Err(status),
    }
}

/// The disjoint `UserEnv` + manager borrow (the `chat_env_split`
/// convention in requests.rs).
fn bot_user_split(td: &mut Td) -> (UserEnv<'_>, &mut crate::user_manager::UserManager) {
    let options = td
        .option_manager
        .as_mut()
        .expect("bot requests run with the initialized option manager");
    let env = UserEnv {
        global: &mut td.global,
        options,
        auth_is_bot: td.auth_manager.is_bot(),
    };
    (env, &mut td.user_manager)
}

impl crate::photo::PhotoStoreEnv for BotEnv<'_> {
    fn store_file_id(&mut self, file_id: FileId, out: &mut Vec<u8>) {
        self.td.file_manager.lock().store_file(file_id, out);
    }

    fn parse_file_id(
        &mut self,
        parser: &mut crate::store_parse::Parser<'_>,
    ) -> Result<FileId, &'static str> {
        self.td.file_manager.lock().parse_file(parser)
    }
}

impl crate::web_app::WebAppEnv for BotEnv<'_> {
    // every WebAppEnv surface is a seam: the document classification is
    // [blocked: Documents], the photo/animation object builders are the
    // FileManager/AnimationsManager object seams
}

impl crate::message_entity::UserManagerLike for BotEnv<'_> {
    fn resolve_user_id_object(&mut self, user_id: UserId, source: &str) -> i64 {
        let (mut env, user_manager) = bot_user_split(self.td);
        user_manager.get_user_id_object(&mut env, user_id, source)
    }

    fn ensure_input_user(&mut self, user_id: UserId) -> Result<(), Status> {
        let (env, user_manager) = bot_user_split(self.td);
        user_manager.get_input_user(&env, user_id).map(|_| ())
    }

    fn have_min_user(&self, user_id: UserId) -> bool {
        self.td.user_manager.have_min_user(user_id)
    }

    fn get_input_user_force(&mut self, user_id: UserId) -> telegram_api::InputUser {
        let (env, user_manager) = bot_user_split(self.td);
        user_manager.get_input_user_force(&env, user_id)
    }

    fn is_user_bot(&self, user_id: UserId) -> bool {
        self.td.user_manager.is_user_bot_of(user_id)
    }
}

impl crate::game::GameEnv for BotEnv<'_> {}

impl crate::web_app_manager::WebAppManagerEnv for BotEnv<'_> {
    fn is_authorized(&self) -> bool {
        self.td.auth_manager.is_authorized()
    }

    fn is_bot(&self) -> bool {
        self.td.auth_manager.is_bot()
    }

    fn close_flag(&self) -> bool {
        self.td.global.close_flag()
    }

    fn unix_time(&self) -> i32 {
        self.td.global.unix_time()
    }

    fn get_input_user(&mut self, bot_user_id: UserId) -> Result<telegram_api::InputUser, Status> {
        let (env, user_manager) = bot_user_split(self.td);
        user_manager.get_input_user(&env, bot_user_id)
    }

    fn get_bot_data(
        &mut self,
        bot_user_id: UserId,
    ) -> Result<crate::user_manager::BotData, Status> {
        self.td.user_manager.get_bot_data(bot_user_id)
    }

    fn is_user_bot(&mut self, user_id: UserId) -> bool {
        self.td.user_manager.is_user_bot_of(user_id)
    }

    fn on_get_user(&mut self, user: telegram_api::User, source: &str) -> UserId {
        let (mut env, user_manager) = bot_user_split(self.td);
        user_manager.on_get_user(&mut env, user, source)
    }

    fn get_user_id_object(&mut self, user_id: UserId, source: &str) -> i64 {
        let (mut env, user_manager) = bot_user_split(self.td);
        user_manager.get_user_id_object(&mut env, user_id, source)
    }

    fn have_input_peer(
        &mut self,
        dialog_id: crate::dialog_id::DialogId,
        access_rights: crate::user_manager::AccessRights,
    ) -> bool {
        crate::dialog_manager::have_input_peer(self.td, dialog_id, false, access_rights)
    }

    fn get_input_peer(
        &mut self,
        dialog_id: crate::dialog_id::DialogId,
        access_rights: crate::user_manager::AccessRights,
    ) -> Option<telegram_api::InputPeer> {
        crate::dialog_manager::get_input_peer(self.td, dialog_id, access_rights)
    }

    fn is_monoforum_channel(&mut self, dialog_id: crate::dialog_id::DialogId) -> bool {
        crate::dialog_manager::is_monoforum_channel(self.td, dialog_id)
    }

    fn is_admined_monoforum_channel(&mut self, dialog_id: crate::dialog_id::DialogId) -> bool {
        crate::dialog_manager::is_admined_monoforum_channel(self.td, dialog_id)
    }

    fn check_dialog_access(
        &mut self,
        dialog_id: crate::dialog_id::DialogId,
        access_rights: crate::user_manager::AccessRights,
        source: &str,
    ) -> Result<(), Status> {
        bot_dialog_result(crate::dialog_manager::check_dialog_access(
            self.td,
            dialog_id,
            false,
            access_rights,
            source,
        ))
    }

    fn on_get_dialog_error(
        &mut self,
        dialog_id: crate::dialog_id::DialogId,
        status: &Status,
        source: &str,
    ) -> bool {
        crate::dialog_manager::on_get_dialog_error(self.td, dialog_id, status, source)
    }

    fn have_dialog_force(&mut self, dialog_id: crate::dialog_id::DialogId, source: &str) -> bool {
        crate::dialog_manager::have_dialog_force(self.td, dialog_id, source)
    }

    fn is_megagroup_channel(&mut self, channel_id: crate::chat_manager::ChannelId) -> bool {
        // `chat_manager_->is_megagroup_channel` — the unloaded-channel
        // C++ result (the stub inventory)
        let _ = channel_id;
        false
    }

    fn create_web_app_file_source(&mut self, user_id: UserId, short_name: &str) -> FileSourceId {
        self.td
            .file_reference_manager
            .intern(crate::files::FileSource::WebApp {
                user_id,
                short_name: short_name.to_owned(),
            })
    }

    fn add_file_source(&mut self, file_id: FileId, file_source_id: FileSourceId, source: &str) {
        let _ = source;
        self.td
            .file_reference_manager
            .add_file_source(file_id, file_source_id);
    }
}

impl crate::callback_queries_manager::CallbackQueriesManagerEnv for BotEnv<'_> {
    fn is_bot(&self) -> bool {
        self.td.auth_manager.is_bot()
    }

    fn close_flag(&mut self) -> bool {
        self.td.global.close_flag()
    }

    fn have_min_user(&mut self, sender_user_id: UserId) -> bool {
        self.td.user_manager.have_min_user(sender_user_id)
    }

    fn get_user_id_object(&mut self, user_id: UserId, source: &str) -> i64 {
        let (mut env, user_manager) = bot_user_split(self.td);
        user_manager.get_user_id_object(&mut env, user_id, source)
    }

    fn send_update(&mut self, update: td_api::Update) {
        self.td.notification_updates.push(update);
    }

    fn force_create_dialog(&mut self, dialog_id: crate::dialog_id::DialogId, source: &str) {
        crate::dialog_manager::force_create_dialog(self.td, dialog_id, source)
    }

    fn get_chat_id_object(&mut self, dialog_id: crate::dialog_id::DialogId, source: &str) -> i64 {
        crate::dialog_manager::get_chat_id_object(dialog_id, source)
    }

    fn check_dialog_access(
        &mut self,
        dialog_id: crate::dialog_id::DialogId,
        source: &str,
    ) -> Result<(), Status> {
        bot_dialog_result(crate::dialog_manager::check_dialog_access(
            self.td,
            dialog_id,
            false,
            crate::user_manager::AccessRights::Read,
            source,
        ))
    }

    fn check_dialog_access_in_memory(
        &mut self,
        dialog_id: crate::dialog_id::DialogId,
    ) -> Result<(), Status> {
        bot_dialog_result(crate::dialog_manager::check_dialog_access_in_memory(
            self.td,
            dialog_id,
            false,
            crate::user_manager::AccessRights::Read,
        ))
    }

    fn on_get_dialog_error(
        &mut self,
        dialog_id: crate::dialog_id::DialogId,
        status: &Status,
        source: &str,
    ) -> bool {
        crate::dialog_manager::on_get_dialog_error(self.td, dialog_id, status, source)
    }

    fn have_message_force(
        &mut self,
        message_full_id: crate::message_id::MessageFullId,
        source: &str,
    ) -> bool {
        let _ = (message_full_id, source);
        tdutils::log_info!("MessagesManager::have_message_force — [blocked: M.10 Messages]");
        false
    }
}

impl crate::bot_recommendation_manager::BotRecommendationManagerEnv for BotEnv<'_> {
    fn is_user_bot(&mut self, bot_user_id: UserId) -> bool {
        self.td.user_manager.is_user_bot_of(bot_user_id)
    }

    fn get_input_user(&mut self, bot_user_id: UserId) -> Result<telegram_api::InputUser, Status> {
        let (env, user_manager) = bot_user_split(self.td);
        user_manager.get_input_user(&env, bot_user_id)
    }

    fn on_get_user(&mut self, user: telegram_api::User, source: &str) -> UserId {
        let (mut env, user_manager) = bot_user_split(self.td);
        user_manager.on_get_user(&mut env, user, source)
    }

    fn get_user_id_object(&mut self, user_id: UserId, source: &str) -> i64 {
        let (mut env, user_manager) = bot_user_split(self.td);
        user_manager.get_user_id_object(&mut env, user_id, source)
    }

    fn get_users_object(&mut self, total_count: i32, user_ids: &[UserId]) -> td_api::Users {
        let (mut env, user_manager) = bot_user_split(self.td);
        user_manager.get_users_object_of(&mut env, total_count, user_ids)
    }

    fn get_option_boolean(&mut self, name: &str) -> bool {
        self.td
            .option_manager
            .as_ref()
            .is_some_and(|options| options.get_option_boolean(name, false))
    }

    fn use_message_database(&mut self) -> bool {
        self.td.global.use_message_database()
    }

    fn close_flag(&mut self) -> bool {
        self.td.global.close_flag()
    }

    fn sqlite_pmc_get_raw(&mut self, key: &str) -> Vec<u8> {
        self.td.global.td_db().common_kv().get(key.as_bytes())
    }

    fn sqlite_pmc_set_raw(&mut self, key: &str, value: &[u8]) {
        self.td
            .global
            .td_db()
            .common_kv()
            .set(key.as_bytes(), value);
    }

    fn sqlite_pmc_erase(&mut self, key: &str) {
        self.td.global.td_db().common_kv().erase(key.as_bytes());
    }
}

impl crate::game_manager::GameManagerEnv for BotEnv<'_> {
    fn is_bot(&self) -> bool {
        self.td.auth_manager.is_bot()
    }

    fn get_input_user(&mut self, user_id: UserId) -> Result<telegram_api::InputUser, Status> {
        let (env, user_manager) = bot_user_split(self.td);
        user_manager.get_input_user(&env, user_id)
    }

    fn on_get_users(&mut self, users: Vec<telegram_api::User>, source: &str) {
        let (mut env, user_manager) = bot_user_split(self.td);
        user_manager.on_get_users(&mut env, users, source)
    }

    fn get_user_id_object(&mut self, user_id: UserId, source: &str) -> i64 {
        let (mut env, user_manager) = bot_user_split(self.td);
        user_manager.get_user_id_object(&mut env, user_id, source)
    }

    fn check_dialog_access(
        &mut self,
        dialog_id: crate::dialog_id::DialogId,
        access_rights: crate::user_manager::AccessRights,
        source: &str,
    ) -> Result<(), Status> {
        bot_dialog_result(crate::dialog_manager::check_dialog_access(
            self.td,
            dialog_id,
            false,
            access_rights,
            source,
        ))
    }

    fn on_get_dialog_error(
        &mut self,
        dialog_id: crate::dialog_id::DialogId,
        status: &Status,
        source: &str,
    ) {
        crate::dialog_manager::on_get_dialog_error(self.td, dialog_id, status, source);
    }

    fn have_message_force(
        &mut self,
        message_full_id: crate::message_id::MessageFullId,
        source: &str,
    ) -> bool {
        let _ = (message_full_id, source);
        tdutils::log_info!("MessagesManager::have_message_force — [blocked: M.10 Messages]");
        false
    }

    fn can_set_game_score(&mut self, message_full_id: crate::message_id::MessageFullId) -> bool {
        let _ = message_full_id;
        tdutils::log_info!("MessagesManager::can_set_game_score — [blocked: M.10 Messages]");
        false
    }

    fn get_message_object(
        &mut self,
        message_full_id: crate::message_id::MessageFullId,
        source: &str,
    ) -> Option<td_api::Message> {
        let _ = (message_full_id, source);
        tdutils::log_info!("MessagesManager::get_message_object — [blocked: M.10 Messages]");
        None
    }

    fn on_get_updates(&mut self, updates: telegram_api::Updates, promise: Promise<()>) {
        let options = self
            .td
            .option_manager
            .as_mut()
            .expect("option manager exists after init");
        let global = &mut self.td.global;
        let mut env = TdUpdatesEnv {
            global,
            options,
            auth_is_authorized: self.td.auth_manager.is_authorized(),
            auth_is_bot: self.td.auth_manager.is_bot(),
            my_id: self.td.user_manager.get_my_id().get(),
            can_ignore_background_updates: self.td.can_ignore_background_updates,
        };
        self.td
            .updates_manager
            .on_get_updates(&mut env, updates, promise);
    }
}

/// The `G()`/`td_` surface for the [`crate::story_manager::StoryManager`]
/// (the M.47 conventions): field-split borrows off `Td`, the notification
/// outbox for `send_update`, and the M.12 file manager behind its shared
/// lock. The StoryDb seam answers `None` — the story tables are implemented
/// standalone in [`crate::story_db`] over the tddb `SqliteDb` (the M.13S
/// `notification_db.rs` precedent); the TdDb story-db plumbing lands at the
/// db integration.
pub(crate) struct StoryEnv<'a> {
    pub global: &'a mut Global,
    pub options: Option<&'a mut OptionManager>,
    pub auth_is_authorized: bool,
    pub auth_is_bot: bool,
    pub user_manager: &'a mut UserManager,
    pub chat_manager: &'a mut crate::chat_manager::ChatManager,
    pub file_manager: std::sync::Arc<parking_lot::Mutex<crate::files::FileManager>>,
    pub updates: &'a mut Vec<td_api::Update>,
}

impl<'a> StoryEnv<'a> {
    /// The `UserEnv` for the user-manager surfaces the story plane needs;
    /// the caller halves land with the user-manager read surface.
    #[allow(dead_code)]
    fn user_env(&mut self) -> UserEnv<'_> {
        UserEnv {
            global: self.global,
            options: self
                .options
                .as_deref_mut()
                .expect("story requests run with the initialized option manager"),
            auth_is_bot: self.auth_is_bot,
        }
    }
}

impl<'a> crate::story_manager::StoryManagerEnv for StoryEnv<'a> {
    fn is_authorized(&self) -> bool {
        self.auth_is_authorized
    }

    fn is_bot(&self) -> bool {
        self.auth_is_bot
    }

    fn use_message_database(&self) -> bool {
        self.global.use_message_database()
    }

    fn unix_time(&mut self) -> i32 {
        self.global.unix_time()
    }

    fn binlog_pmc_get_raw(&mut self, key: &[u8]) -> Vec<u8> {
        self.global.binlog_pmc().get_raw(key)
    }

    fn binlog_pmc_set_raw(&mut self, key: &[u8], value: &[u8]) {
        self.global.binlog_pmc().set_raw(key, value);
    }

    fn binlog_pmc_erase(&mut self, key: &[u8]) {
        // BinlogKeyValue::erase is spelled &str; the story keys are ASCII.
        self.global
            .binlog_pmc()
            .erase(&String::from_utf8_lossy(key));
    }

    fn get_option_integer(&mut self, name: &str, default_value: i64) -> i64 {
        match self.options.as_deref_mut() {
            Some(options) => options.get_option_integer(name, default_value),
            None => default_value,
        }
    }

    fn get_option_boolean(&mut self, name: &str) -> bool {
        match self.options.as_deref_mut() {
            Some(options) => options.get_option_boolean(name, false),
            None => false,
        }
    }

    fn set_option_empty(&mut self, name: &str) {
        if let Some(options) = self.options.as_deref_mut() {
            options.set_option_empty(name);
        }
    }

    fn send_update(&mut self, update: td_api::Update) {
        self.updates.push(update);
    }

    fn on_get_users(&mut self, users: Vec<tdmtproto::telegram_api::User>, source: &str) {
        // Field-split borrows: the UserEnv holds `global`/`options` while
        // `user_manager` is borrowed separately (the E0499 split).
        let StoryEnv {
            global,
            options,
            auth_is_bot,
            user_manager,
            ..
        } = self;
        let mut user_env = UserEnv {
            global,
            options: options
                .as_deref_mut()
                .expect("story requests run with the initialized option manager"),
            auth_is_bot: *auth_is_bot,
        };
        user_manager.on_get_users(&mut user_env, users, source);
    }

    fn on_get_chats(&mut self, chats: Vec<tdmtproto::telegram_api::Chat>, source: &str) {
        let StoryEnv {
            global,
            options,
            auth_is_bot,
            user_manager,
            chat_manager,
            ..
        } = self;
        let mut user_env = UserEnv {
            global,
            options: options
                .as_deref_mut()
                .expect("story requests run with the initialized option manager"),
            auth_is_bot: *auth_is_bot,
        };
        chat_manager.on_get_chats(user_manager, &mut user_env, chats, source);
    }

    fn force_create_dialog(
        &mut self,
        dialog_id: crate::dialog_id::DialogId,
        _source: &str,
        _force: bool,
    ) {
        // `DialogManager::force_create_dialog` — the M.6 stub is a no-op
        let _ = dialog_id;
    }

    fn check_dialog_access(
        &mut self,
        dialog_id: crate::dialog_id::DialogId,
        allow_secret_chats: bool,
        need_write_access: bool,
        source: &str,
    ) -> tdutils::status::Result<()> {
        if !self.have_dialog_force(dialog_id, source) {
            if !dialog_id.is_valid() {
                return tdutils::status::Result::from_status(Status::error(
                    400,
                    "Invalid chat identifier specified",
                ));
            }
            return tdutils::status::Result::from_status(Status::error(400, "Chat not found"));
        }
        // `check_dialog_access_in_memory` over the env surfaces
        if !self.have_input_peer(dialog_id, allow_secret_chats, need_write_access) {
            if dialog_id.get_type() == crate::dialog_id::DialogType::SecretChat
                && !allow_secret_chats
            {
                return tdutils::status::Result::from_status(Status::error(
                    400,
                    "Not supported in secret chats",
                ));
            }
            if need_write_access {
                return tdutils::status::Result::from_status(Status::error(
                    400,
                    "Have no write access to the chat",
                ));
            }
            return tdutils::status::Result::from_status(Status::error(
                400,
                "Can't access the chat",
            ));
        }
        tdutils::status::Result::Ok(())
    }

    fn have_dialog_force(&mut self, dialog_id: crate::dialog_id::DialogId, source: &str) -> bool {
        let _ = source;
        match dialog_id.get_type() {
            crate::dialog_id::DialogType::User => self
                .user_manager
                .have_accessible_user(dialog_id.get_user_id()),
            crate::dialog_id::DialogType::Chat | crate::dialog_id::DialogType::Channel => false,
            crate::dialog_id::DialogType::SecretChat => self
                .user_manager
                .have_secret_chat(dialog_id.get_secret_chat_id()),
            crate::dialog_id::DialogType::None => false,
        }
    }

    fn have_input_peer(
        &mut self,
        dialog_id: crate::dialog_id::DialogId,
        allow_secret_chats: bool,
        need_write: bool,
    ) -> bool {
        let access_rights = if need_write {
            crate::user_manager::AccessRights::Write
        } else {
            crate::user_manager::AccessRights::Read
        };
        match dialog_id.get_type() {
            crate::dialog_id::DialogType::User => {
                let StoryEnv {
                    global,
                    options,
                    auth_is_bot,
                    user_manager,
                    ..
                } = self;
                let user_env = UserEnv {
                    global,
                    options: options
                        .as_deref_mut()
                        .expect("story requests run with the initialized option manager"),
                    auth_is_bot: *auth_is_bot,
                };
                user_manager.have_input_peer_user(&user_env, dialog_id.get_user_id(), access_rights)
            }
            // ChatManager peers arrive with its read surface (stub inventory)
            crate::dialog_id::DialogType::Chat | crate::dialog_id::DialogType::Channel => false,
            crate::dialog_id::DialogType::SecretChat => {
                allow_secret_chats
                    && self
                        .user_manager
                        .have_input_encrypted_peer(dialog_id.get_secret_chat_id(), access_rights)
            }
            crate::dialog_id::DialogType::None => false,
        }
    }

    fn get_my_dialog_id(&self) -> crate::dialog_id::DialogId {
        crate::dialog_id::DialogId::from_user_id(self.user_manager.get_my_id())
    }

    fn get_chat_id_object(&mut self, dialog_id: crate::dialog_id::DialogId, source: &str) -> i64 {
        crate::dialog_manager::get_chat_id_object(dialog_id, source)
    }

    fn is_group_dialog(&mut self, dialog_id: crate::dialog_id::DialogId) -> bool {
        dialog_id.get_type() == crate::dialog_id::DialogType::Chat
    }

    fn get_my_id(&self) -> crate::user_manager::UserId {
        self.user_manager.get_my_id()
    }

    fn have_min_user(&self, user_id: crate::user_manager::UserId) -> bool {
        self.user_manager.have_min_user(user_id)
    }

    fn get_user_ids_object(
        &mut self,
        user_ids: &[crate::user_manager::UserId],
        source: &str,
    ) -> Vec<i64> {
        let _ = source;
        user_ids.iter().map(|user_id| user_id.get()).collect()
    }

    fn is_user_bot(&self, user_id: crate::user_manager::UserId) -> bool {
        self.user_manager.is_user_bot_of(user_id)
    }

    fn is_user_premium(&self, user_id: crate::user_manager::UserId) -> bool {
        self.user_manager.is_user_premium_of(user_id)
    }

    fn is_user_contact(&self, user_id: crate::user_manager::UserId) -> bool {
        self.user_manager.is_user_contact_of(user_id, false)
    }

    fn get_user_stories_hidden(&self, user_id: crate::user_manager::UserId) -> bool {
        self.user_manager.get_user_stories_hidden_of(user_id)
    }

    fn get_channel_status(&self, channel_id: i64) -> crate::chat_manager::DialogParticipantStatus {
        self.chat_manager
            .get_channel_status(crate::chat_manager::ChannelId::new(channel_id))
    }

    fn is_megagroup_channel(&self, channel_id: i64) -> bool {
        // `get_channel_type(channel_id) == ChannelType::Megagroup`
        // (StoryManager.cpp:4086).
        self.chat_manager
            .get_channel_type_by_id(crate::chat_manager::ChannelId::new(channel_id))
            == crate::chat_manager::ChannelType::Megagroup
    }

    fn get_input_file_id(
        &mut self,
        file_type: crate::files::FileType,
        file: Option<&td_api::InputFile>,
        owner_dialog_id: crate::dialog_id::DialogId,
        allow_zero: bool,
        is_encrypted: bool,
    ) -> tdutils::status::Result<crate::files::FileId> {
        // files.rs rides the `message_id::DialogId` spelling; convert.
        let owner_dialog_id = crate::message_id::DialogId::new(owner_dialog_id.get());
        let mut file_manager = self.file_manager.lock();
        tdutils::status::Result::from(file_manager.get_input_file_id(
            file_type,
            file,
            owner_dialog_id,
            allow_zero,
            is_encrypted,
            false,
            false,
            false,
            false,
        ))
    }

    fn copy_file_id(
        &mut self,
        file_id: crate::files::FileId,
        file_type: crate::files::FileType,
        owner_dialog_id: crate::dialog_id::DialogId,
        source: &str,
    ) -> crate::files::FileId {
        let owner_dialog_id = crate::message_id::DialogId::new(owner_dialog_id.get());
        let mut file_manager = self.file_manager.lock();
        file_manager.copy_file_id(file_id, file_type, owner_dialog_id, source)
    }

    fn videos_store_video(&mut self, file_id: crate::files::FileId, out: &mut Vec<u8>) {
        let file_manager = self.file_manager.lock();
        file_manager.store_file(file_id, out);
    }

    fn videos_parse_video(
        &mut self,
        parser: &mut crate::store_parse::Parser,
    ) -> std::result::Result<crate::files::FileId, &'static str> {
        let mut file_manager = self.file_manager.lock();
        file_manager.parse_file(parser)
    }

    fn videos_get_video_duration(&mut self, file_id: crate::files::FileId) -> i32 {
        // `VideosManager::get_video_duration` reads the video record —
        // [blocked: VideosManager]; the interim answer is the C++ "not a
        // video" -1
        let _ = file_id;
        -1
    }

    fn story_db(&mut self) -> Option<&mut crate::story_db::StoryDb> {
        // [blocked: TdDb story-db plumbing at integration] — the story
        // tables live standalone in `crate::story_db` until then
        None
    }
}

/// The `G()`/`td_` slice the forum plane reads through (the M.49
/// convention): field-split borrows off `Td`. The [blocked: net] query
/// tails and the notification-settings/draft halves keep their trait
/// defaults until their tracks land.
pub(crate) struct ForumTopicTdEnv<'a> {
    pub(crate) global: &'a mut Global,
    pub(crate) auth_is_bot: bool,
    /// The chat-manager half for the forum checks — kept for the C++
    /// field parity; the is_forum read lands with the chat plane.
    #[allow(dead_code)]
    pub(crate) chat_manager: &'a mut crate::chat_manager::ChatManager,
    pub(crate) updates: &'a mut Vec<td_api::Update>,
}

impl crate::forum_topic_manager::ForumTopicEnv for ForumTopicTdEnv<'_> {
    fn is_bot(&self) -> bool {
        self.auth_is_bot
    }

    fn unix_time(&mut self) -> i32 {
        self.global.unix_time()
    }

    fn send_update(&mut self, update: td_api::Update) {
        self.updates.push(update);
    }
}

/// The `G()`/`td_` slice the secret-chats plane reads through (the M.50
/// convention): field-split borrows off `Td`. The [blocked: net] tails
/// and the tde2e actor halves keep their trait defaults.
pub(crate) struct SecretChatsTdEnv<'a> {
    /// The C++ `G()` reads — kept for the field parity; the halves land
    /// with the tde2e actor plane.
    #[allow(dead_code)]
    pub(crate) global: &'a mut Global,
    pub(crate) auth_is_authorized: bool,
    pub(crate) use_secret_chats: bool,
    /// The user-manager half for the access-hash read — kept for the
    /// C++ field parity.
    #[allow(dead_code)]
    pub(crate) user_manager: &'a mut UserManager,
    pub(crate) updates: &'a mut Vec<td_api::Update>,
}

impl crate::secret_chats_manager::SecretChatsEnv for SecretChatsTdEnv<'_> {
    fn is_authorized(&self) -> bool {
        self.auth_is_authorized
    }

    fn use_secret_chats(&self) -> bool {
        self.use_secret_chats
    }

    fn send_update(&mut self, update: td_api::Update) {
        self.updates.push(update);
    }
}

/// The `G()`/`td_` slice the group-call plane reads through (the M.51
/// convention): field-split borrows off `Td`. Every query tail is a
/// [blocked: net] seam (D4 signaling-only); the E2E-key functions are
/// [blocked: tde2e].
pub(crate) struct GroupCallTdEnv<'a> {
    /// The C++ `G()` reads — kept for the field parity; the halves land
    /// with the media-transport plane.
    #[allow(dead_code)]
    pub(crate) global: &'a mut Global,
    pub(crate) auth_is_authorized: bool,
    pub(crate) auth_is_bot: bool,
    pub(crate) updates: &'a mut Vec<td_api::Update>,
}

impl crate::group_call_manager::GroupCallEnv for GroupCallTdEnv<'_> {
    fn is_authorized(&self) -> bool {
        self.auth_is_authorized
    }

    fn is_bot(&self) -> bool {
        self.auth_is_bot
    }

    fn send_update(&mut self, update: td_api::Update) {
        self.updates.push(update);
    }
}

/// The `G()`/`td_` slice the referral plane reads through (the M.52
/// convention): field-split borrows off `Td`. Every server leg is a
/// [blocked: net] seam.
pub(crate) struct ReferralTdEnv<'a> {
    /// The C++ `G()` reads — kept for the field parity; the halves land
    /// with the net step.
    #[allow(dead_code)]
    pub(crate) global: &'a mut Global,
    pub(crate) updates: &'a mut Vec<td_api::Update>,
}

impl crate::referral_program::ReferralProgramEnv for ReferralTdEnv<'_> {
    fn send_update(&mut self, update: td_api::Update) {
        self.updates.push(update);
    }
}

/// The `G()`/`td_` slice the business plane reads through (the M.48
/// convention): field-split borrows off `Td`. The [blocked: net] query
/// tails, the sticker/file-view halves and the messages-manager halves
/// keep their trait defaults until their tracks land.
pub(crate) struct BusinessTdEnv<'a> {
    pub(crate) global: &'a mut Global,
    pub(crate) options: Option<&'a mut OptionManager>,
    pub(crate) auth_is_bot: bool,
    pub(crate) time_zone_manager: &'a mut TimeZoneManager,
    pub(crate) user_manager: &'a mut UserManager,
    /// The chat-manager half for the chat-link/recipient answer paths —
    /// kept for the C++ field parity; the reads land with the chat plane.
    #[allow(dead_code)]
    pub(crate) chat_manager: &'a mut crate::chat_manager::ChatManager,
}

impl crate::business_manager::BusinessEnv for BusinessTdEnv<'_> {
    // NOTE: the trait signatures use crate::dialog_id::DialogId (the
    // canonical M.12 id space) while td.rs imports message_id::DialogId —
    // qualify explicitly (the two-DialogIds convention).

    fn is_bot(&self) -> bool {
        self.auth_is_bot
    }

    fn get_time_zone_offset(&mut self, time_zone_id: &str) -> i32 {
        let global = &mut *self.global;
        let options = self
            .options
            .as_ref()
            .expect("business requests run with the initialized option manager");
        self.time_zone_manager
            .get_time_zone_offset(global, options, time_zone_id)
    }

    fn get_option_integer(&mut self, name: &str, default_value: i64) -> i64 {
        self.options.as_ref().map_or(default_value, |options| {
            options.get_option_integer(name, default_value)
        })
    }

    fn force_create_dialog(
        &mut self,
        dialog_id: crate::dialog_id::DialogId,
        source: &str,
        force: bool,
    ) {
        // `DialogManager::force_create_dialog` — the master dialog plane
        // stub is a no-op; the real force-create flips at the dialog step.
        tdutils::log_info!(
            "force_create_dialog({}, {source}, {force}) — the dialog-plane stub",
            dialog_id.get()
        );
    }

    fn get_chat_id_object(&mut self, dialog_id: crate::dialog_id::DialogId, source: &str) -> i64 {
        crate::dialog_manager::get_chat_id_object(dialog_id, source)
    }

    fn get_input_users_force(
        &mut self,
        user_ids: &[crate::user_manager::UserId],
    ) -> Vec<telegram_api::InputUser> {
        let user_env = crate::user_manager::UserEnv {
            global: &mut *self.global,
            options: self
                .options
                .as_deref_mut()
                .expect("business requests run with the initialized option manager"),
            auth_is_bot: self.auth_is_bot,
        };
        self.user_manager.get_input_users_force(&user_env, user_ids)
    }

    fn get_user_id_object(&mut self, user_id: crate::user_manager::UserId, source: &str) -> i64 {
        let mut user_env = crate::user_manager::UserEnv {
            global: &mut *self.global,
            options: self
                .options
                .as_deref_mut()
                .expect("business requests run with the initialized option manager"),
            auth_is_bot: self.auth_is_bot,
        };
        self.user_manager
            .get_user_id_object(&mut user_env, user_id, source)
    }

    fn get_input_user(
        &mut self,
        user_id: crate::user_manager::UserId,
    ) -> std::result::Result<telegram_api::InputUser, tdutils::status::Status> {
        let user_env = crate::user_manager::UserEnv {
            global: &mut *self.global,
            options: self
                .options
                .as_deref_mut()
                .expect("business requests run with the initialized option manager"),
            auth_is_bot: self.auth_is_bot,
        };
        self.user_manager.get_input_user(&user_env, user_id)
    }
}

/// Field-split the `Td` for the WebPagesManager calls.
pub(crate) fn web_pages_env_split(
    td: &mut Td,
) -> (
    TdWebPagesEnv<'_>,
    &mut crate::web_pages_manager::WebPagesManager,
) {
    let crate::td::Td {
        global,
        option_manager,
        auth_manager,
        user_manager,
        file_manager,
        file_reference_manager,
        web_pages_manager,
        web_pages_timeouts,
        ..
    } = td;
    (
        TdWebPagesEnv {
            global,
            options: option_manager.as_mut(),
            auth_is_bot: auth_manager.is_bot(),
            user_manager,
            file_manager: std::sync::Arc::clone(file_manager),
            file_reference_manager,
            web_pages_timeouts,
        },
        web_pages_manager,
    )
}

/// The `G()`/manager slice of the collapsed WebPagesManager (M.17).
pub(crate) struct TdWebPagesEnv<'a> {
    pub(crate) global: &'a mut Global,
    /// `None` once the option manager is torn down; the getters answer
    /// their defaults on the destroy path.
    pub(crate) options: Option<&'a mut OptionManager>,
    pub(crate) auth_is_bot: bool,
    pub(crate) user_manager: &'a mut UserManager,
    pub(crate) file_manager: std::sync::Arc<parking_lot::Mutex<FileManager>>,
    pub(crate) file_reference_manager: &'a mut crate::files::FileReferenceManager,
    pub(crate) web_pages_timeouts: &'a mut std::collections::HashMap<i64, f64>,
}

impl crate::photo::PhotoStoreEnv for TdWebPagesEnv<'_> {
    fn store_file_id(&mut self, file_id: FileId, out: &mut Vec<u8>) {
        self.file_manager.lock().store_file(file_id, out);
    }

    fn parse_file_id(
        &mut self,
        parser: &mut crate::store_parse::Parser<'_>,
    ) -> Result<FileId, &'static str> {
        self.file_manager.lock().parse_file(parser)
    }
}

impl crate::webpage::WebPageBlockEnv for TdWebPagesEnv<'_> {
    fn get_document_object(&self, _file_id: FileId) -> Option<td_api::Document> {
        None // [blocked: M.28 DocumentsManager object]
    }

    fn get_animation_object(&self, _file_id: FileId) -> Option<td_api::Animation> {
        None // [blocked: M.20]
    }

    fn get_audio_object(&self, _file_id: FileId) -> Option<td_api::Audio> {
        None // [blocked: M.28]
    }

    fn get_video_object(&self, _file_id: FileId) -> Option<td_api::Video> {
        None // [blocked: M.28]
    }

    fn get_video_note_object(&self, _file_id: FileId) -> Option<td_api::VideoNote> {
        None // [blocked: M.28]
    }

    fn get_voice_note_object(&self, _file_id: FileId) -> Option<td_api::VoiceNote> {
        None // [blocked: M.28]
    }

    fn get_accent_color_id_object(
        &self,
        accent_color_id: crate::peer_color::AccentColorId,
        fallback_accent_color_id: crate::peer_color::AccentColorId,
    ) -> Option<i32> {
        // ThemeManager::get_accent_color_id_object (ThemeManager.cpp:639):
        // the is_built_in/light_colors_ server-data halves are [blocked:
        // M.41 — the theme color sets], so the plain validity check stands
        // in for the acceptance gate.
        if accent_color_id.is_valid() {
            return Some(accent_color_id.get());
        }
        if !fallback_accent_color_id.is_valid() {
            return Some(5); // blue
        }
        Some(fallback_accent_color_id.get())
    }

    fn get_chat_photo_info_object(
        &self,
        _photo: &crate::photo::DialogPhoto,
    ) -> Option<td_api::ChatPhotoInfo> {
        None // [blocked: M.6]
    }

    fn get_instant_view_link(&self, url: &str, rhash: &str) -> String {
        crate::link_manager::get_instant_view_link(
            &TdStaticLinkEnv {
                options: self.options.as_deref(),
            },
            url,
            rhash,
        )
    }

    fn get_instant_view_link_rhash(&self, real_url: &str) -> String {
        crate::link_manager::get_instant_view_link_rhash(
            &TdStaticLinkEnv {
                options: self.options.as_deref(),
            },
            real_url,
        )
    }

    fn get_instant_view_link_url(&self, real_url: &str) -> String {
        crate::link_manager::get_instant_view_link_url(
            &TdStaticLinkEnv {
                options: self.options.as_deref(),
            },
            real_url,
        )
    }

    fn as_input_document(
        &mut self,
        _file_id: FileId,
    ) -> Option<(i64, telegram_api::InputDocument)> {
        None // [blocked: M.12 file-reference plane]
    }

    fn as_input_photo(&mut self, _file_id: FileId) -> Option<(i64, telegram_api::InputPhoto)> {
        None // [blocked: M.12]
    }

    fn ensure_input_user(&mut self, user_id: crate::user_manager::UserId) -> Result<(), Status> {
        // Td::ensure_input_user — the get_input_user validation half.
        let Some(options) = self.options.as_mut() else {
            return Err(Status::error(400, "Have no access to the user"));
        };
        let env = UserEnv {
            global: self.global,
            options,
            auth_is_bot: self.auth_is_bot,
        };
        self.user_manager.get_input_user(&env, user_id).map(|_| ())
    }

    fn get_user_id_object(&mut self, user_id: crate::user_manager::UserId, source: &str) -> i64 {
        let Some(options) = self.options.as_mut() else {
            return user_id.get();
        };
        let mut env = UserEnv {
            global: self.global,
            options,
            auth_is_bot: self.auth_is_bot,
        };
        self.user_manager
            .get_user_id_object(&mut env, user_id, source)
    }

    fn is_my_user_bot(&self) -> bool {
        self.user_manager
            .is_user_bot_of(self.user_manager.get_my_id())
    }

    fn on_get_page_block_channel(
        &mut self,
        channel: telegram_api::channel,
    ) -> Option<crate::webpage::ChatLinkInfo> {
        // WebPageId.cpp:5128 — the pageBlockChannel intake. The known-channel
        // half (the on_get_chat store refresh + the get_channel_* accessors)
        // is [blocked: M.5] and its force-get is [blocked: net], so both
        // branches read the min-channel struct like the C++ unknown path;
        // the photo's file registration is the documented Files-step no-op.
        let channel_id = crate::chat_manager::ChannelId::new(channel.id);
        if !channel_id.is_valid() {
            tdutils::log_error!("Receive invalid channel id {}", channel.id);
            return None;
        }
        let accent_color_id = channel
            .color
            .as_ref()
            .map(crate::peer_color::PeerColor::from_telegram)
            .map(|color| color.accent_color_id())
            .unwrap_or_default();
        Some(crate::webpage::ChatLinkInfo {
            title: channel.title,
            photo: crate::photo::DialogPhoto::get_dialog_photo(channel.photo),
            username: channel.username,
            accent_color_id,
            channel_id,
        })
    }
}

impl crate::web_pages_manager::WebPagesManagerEnv for TdWebPagesEnv<'_> {
    fn auth_is_bot(&self) -> bool {
        self.auth_is_bot
    }

    fn close_flag(&self) -> bool {
        self.global.close_flag()
    }

    fn use_message_database(&self) -> bool {
        self.global.use_message_database()
    }

    fn server_time(&self) -> f64 {
        self.global.server_time()
    }

    fn sqlite_pmc_get_raw(&mut self, key: &[u8]) -> Vec<u8> {
        // G()->td_db()->get_sqlite_pmc() — the common_kv stand-in.
        self.global.td_db().common_kv().get(key)
    }

    fn sqlite_pmc_set_raw(&mut self, key: &[u8], value: &[u8]) {
        self.global.td_db().common_kv().set(key, value);
    }

    fn sqlite_pmc_erase(&mut self, key: &[u8]) {
        self.global.td_db().common_kv().erase(key);
    }

    fn binlog_add_web_page(&mut self, data: &[u8]) -> u64 {
        self.global.td_db().binlog().add(
            crate::web_pages_manager::LOG_EVENT_HANDLER_TYPE_WEB_PAGES,
            data,
        )
    }

    fn binlog_rewrite_web_page(&mut self, event_id: u64, data: &[u8]) {
        self.global.td_db().binlog().rewrite(
            event_id,
            crate::web_pages_manager::LOG_EVENT_HANDLER_TYPE_WEB_PAGES,
            data,
        );
    }

    fn binlog_erase(&mut self, event_id: u64) {
        self.global.td_db().binlog().erase(event_id);
    }

    fn add_pending_web_page_timeout(&mut self, web_page_id: i64, timeout_in: f64) {
        let deadline = tdutils::time::now() + timeout_in;
        self.web_pages_timeouts
            .entry(web_page_id)
            .and_modify(|old| {
                if deadline < *old {
                    *old = deadline;
                }
            })
            .or_insert(deadline);
    }

    fn cancel_pending_web_page_timeout(&mut self, web_page_id: i64) {
        self.web_pages_timeouts.remove(&web_page_id);
    }

    fn create_web_page_file_source(&mut self, url: &str) -> FileSourceId {
        self.file_reference_manager.create_web_page_file_source(url)
    }

    fn change_files_source(
        &mut self,
        source_id: FileSourceId,
        old_file_ids: &[FileId],
        new_file_ids: &[FileId],
        source: &str,
    ) {
        self.file_reference_manager.change_files_source(
            source_id,
            old_file_ids,
            new_file_ids,
            source,
        );
    }

    fn delete_pending_message_web_page(&mut self, _message_full_id: MessageFullId) {
        // [blocked: M.10]
    }

    fn on_external_update_message_content(
        &mut self,
        _message_full_id: MessageFullId,
        _source: &str,
    ) {
        // [blocked: M.10]
    }

    fn delete_quick_reply_pending_message_web_page(
        &mut self,
        _message_full_id: QuickReplyMessageFullId,
    ) {
        // [blocked: Wave 3]
    }

    fn quick_reply_on_external_update_message_content(
        &mut self,
        _message_full_id: QuickReplyMessageFullId,
        _source: &str,
    ) {
        // [blocked: Wave 3]
    }

    fn get_messages_from_server(&mut self, _message_full_ids: &[MessageFullId]) {
        // [blocked: M.10/net]
    }

    fn reload_quick_reply_message(
        &mut self,
        _shortcut_id: QuickReplyShortcutId,
        _message_id: crate::message_id::MessageId,
    ) {
        // [blocked: Wave 3]
    }

    fn force_create_dialog(&mut self, _dialog_id: crate::dialog_id::DialogId) {
        // [blocked: M.6]
    }

    fn on_get_story(
        &mut self,
        _dialog_id: crate::dialog_id::DialogId,
        _story: Box<telegram_api::StoryItem>,
    ) -> Option<i32> {
        None // [blocked: Wave 3]
    }

    fn get_chat_id_object(&self, dialog_id: crate::dialog_id::DialogId, _source: &str) -> i64 {
        dialog_id.get_chat_id().get()
    }

    fn get_story_duration(&self, _story_full_id: StoryFullId) -> i32 {
        -1 // [blocked: Wave 3]
    }

    fn on_get_sticker_document(&mut self, _document: Box<telegram_api::Document>) -> FileId {
        FileId::default() // [blocked: M.15]
    }

    fn on_get_document(
        &mut self,
        _document: Box<telegram_api::Document>,
        _owner_dialog_id: crate::dialog_id::DialogId,
    ) -> Option<crate::document::Document> {
        None // [blocked: M.28]
    }

    fn get_document_mime_type(&self, _file_id: FileId) -> String {
        String::new() // [blocked: M.28]
    }

    fn get_message_link_media_timestamp(&self, _url: &str) -> i32 {
        0 // [blocked: M.23]
    }

    fn get_background_type_object(
        &self,
        _url: &str,
        _is_pattern: bool,
    ) -> Option<td_api::BackgroundType> {
        None // [blocked: M.23]
    }

    fn has_video_chat_invite_hash(&self, _url: &str) -> bool {
        false // [blocked: M.23]
    }

    fn is_premium(&self) -> bool {
        self.options
            .as_ref()
            .is_some_and(|options| options.get_option_boolean("is_premium", false))
    }

    fn send_get_web_page_preview_query(
        &mut self,
        _text: crate::message_entity::FormattedText,
        _options: Box<crate::web_pages_manager::GetWebPagePreviewOptions>,
        _promise: Promise<Option<td_api::LinkPreview>>,
    ) {
        // [blocked: net]
    }

    fn send_get_web_page_query(
        &mut self,
        _web_page_id: crate::webpage::WebPageId,
        _url: String,
        _hash: i32,
        _promise: Promise<crate::webpage::WebPageId>,
    ) {
        // [blocked: net]
    }
}

/// Field-split the `Td` for the inline manager calls.
pub(crate) fn inline_env_split(
    td: &mut Td,
) -> (
    TdInlineEnv<'_>,
    &mut crate::inline_queries_manager::InlineQueriesManager,
    &mut crate::inline_message_manager::InlineMessageManager,
) {
    let crate::td::Td {
        global,
        option_manager,
        auth_manager,
        user_manager,
        chat_manager,
        inline_queries_manager,
        inline_message_manager,
        inline_updates,
        ..
    } = td;
    (
        TdInlineEnv {
            global,
            options: option_manager.as_mut(),
            auth_is_bot: auth_manager.is_bot(),
            user_manager,
            chat_manager,
            updates: inline_updates,
        },
        inline_queries_manager,
        inline_message_manager,
    )
}

/// The `G()`/manager slice of the collapsed InlineQueriesManager +
/// InlineMessageManager pair (M.21).
pub(crate) struct TdInlineEnv<'a> {
    pub(crate) global: &'a mut Global,
    /// `None` once the option manager is torn down; the getters answer
    /// their defaults on the destroy path.
    pub(crate) options: Option<&'a mut OptionManager>,
    pub(crate) auth_is_bot: bool,
    pub(crate) user_manager: &'a mut UserManager,
    pub(crate) chat_manager: &'a mut ChatManager,
    pub(crate) updates: &'a mut Vec<td_api::Update>,
}

/// Build the UserEnv from the env's fields (the field-level borrows
/// must be written inline at each call site; a `&mut self` helper would
/// pin the whole struct).
fn make_user_env<'a>(
    global: &'a mut Global,
    options: &'a mut OptionManager,
    auth_is_bot: bool,
) -> UserEnv<'a> {
    UserEnv {
        global,
        options,
        auth_is_bot,
    }
}

impl crate::inline_queries_manager::InlineQueriesManagerEnv for TdInlineEnv<'_> {
    fn auth_is_bot(&self) -> bool {
        self.auth_is_bot
    }

    fn close_flag(&self) -> bool {
        self.global.close_flag()
    }

    fn use_chat_info_database(&self) -> bool {
        self.global.use_chat_info_database()
    }

    fn is_test_dc(&self) -> bool {
        self.global.is_test_dc()
    }

    fn unix_time(&self) -> i32 {
        self.global.unix_time()
    }

    fn get_option_string(&self, name: &str, default_value: &str) -> String {
        self.options.as_ref().map_or_else(
            || default_value.to_owned(),
            |options| options.get_option_string(name, default_value),
        )
    }

    fn binlog_pmc_get(&mut self, key: &str) -> String {
        self.global.binlog_pmc().get(key)
    }

    fn binlog_pmc_set(&mut self, key: &str, value: &str) {
        self.global.binlog_pmc().set(key, value);
    }

    fn send_update(&mut self, update: td_api::Update) {
        self.updates.push(update);
    }

    fn get_bot_data(
        &self,
        user_id: crate::user_manager::UserId,
    ) -> Result<crate::user_manager::BotData, tdutils::status::Status> {
        self.user_manager.get_bot_data(user_id)
    }

    fn have_min_user(&self, user_id: crate::user_manager::UserId) -> bool {
        self.user_manager.have_min_user(user_id)
    }

    fn have_user_force(&self, user_id: crate::user_manager::UserId) -> bool {
        // The forced fetch is the M.4 seam; the in-memory band answers.
        self.user_manager.have_min_user(user_id)
    }

    fn get_user_first_username(&self, user_id: crate::user_manager::UserId) -> String {
        self.user_manager
            .get_user_first_username_of(user_id)
            .to_owned()
    }

    fn get_user_id_object(&mut self, user_id: crate::user_manager::UserId, source: &str) -> i64 {
        let Some(options) = self.options.as_mut() else {
            return user_id.get();
        };
        let mut env = make_user_env(self.global, options, self.auth_is_bot);
        self.user_manager
            .get_user_id_object(&mut env, user_id, source)
    }

    fn get_input_user(
        &mut self,
        user_id: crate::user_manager::UserId,
    ) -> Result<telegram_api::InputUser, tdutils::status::Status> {
        let Some(options) = self.options.as_mut() else {
            return Err(tdutils::status::Status::error(400, "User not found"));
        };
        let env = make_user_env(self.global, options, self.auth_is_bot);
        self.user_manager.get_input_user(&env, user_id)
    }

    fn on_get_users(&mut self, users: Vec<telegram_api::User>, source: &str) {
        let Some(options) = self.options.as_mut() else {
            return;
        };
        let mut env = make_user_env(self.global, options, self.auth_is_bot);
        self.user_manager.on_get_users(&mut env, users, source);
    }

    fn get_input_peer(
        &mut self,
        dialog_id: crate::dialog_id::DialogId,
    ) -> Option<telegram_api::InputPeer> {
        match dialog_id.get_type() {
            crate::dialog_id::DialogType::User => {
                let user_id = dialog_id.get_user_id();
                let options = self.options.as_mut()?;
                let env = make_user_env(self.global, options, self.auth_is_bot);
                self.user_manager
                    .get_input_peer_user(&env, user_id, AccessRights::Read)
            }
            crate::dialog_id::DialogType::Chat
            | crate::dialog_id::DialogType::Channel
            | crate::dialog_id::DialogType::SecretChat => None,
            crate::dialog_id::DialogType::None => Some(telegram_api::InputPeer::inputPeerEmpty(
                telegram_api::inputPeerEmpty {},
            )),
        }
    }

    fn get_channel_type(&self, channel_id: ChannelId) -> ChannelType {
        self.chat_manager.get_channel_type_by_id(channel_id)
    }

    fn is_broadcast_channel(&self, channel_id: ChannelId) -> bool {
        self.chat_manager.get_channel_type_by_id(channel_id) == ChannelType::Broadcast
    }

    fn resolve_weather_dialog(
        &mut self,
        _username: &str,
        _location: crate::chat_manager::Location,
        _promise: Promise<td_api::CurrentWeather>,
    ) {
        // [blocked: net]: the resolve never sends; the promise parks.
    }

    fn can_add_web_page_previews(&self, _dialog_id: crate::dialog_id::DialogId) -> bool {
        false // the M.5 band
    }

    fn get_my_dialog_id(&self) -> crate::dialog_id::DialogId {
        crate::dialog_id::DialogId::from_user_id(self.user_manager.get_my_id())
    }

    fn parse_formatted_text(
        &mut self,
        dialog_id: crate::dialog_id::DialogId,
        text: Option<td_api::FormattedText>,
        is_bot: bool,
        allow_empty: bool,
        skip_media_timestamps: bool,
        skip_trim: bool,
    ) -> Result<crate::message_entity::FormattedText, tdutils::status::Status> {
        let (always_parse_markdown, session_count) = match self.options.as_ref() {
            Some(options) => (
                options.get_option_boolean("always_parse_markdown", false),
                options.get_option_integer("session_count", -1) as i32,
            ),
            None => (false, 0),
        };
        // The real user-manager form rides the M.8 line; the null form is
        // the C++ `user_manager == nullptr` band.
        crate::message_entity::get_formatted_text_of_input::<()>(
            None,
            crate::message_content::to_message_entity_dialog_id(dialog_id),
            text,
            is_bot,
            allow_empty,
            skip_media_timestamps,
            skip_trim,
            None,
            always_parse_markdown,
            session_count,
            None,
            false,
        )
    }

    fn get_input_message_entities(
        &mut self,
        entities: &[crate::message_entity::MessageEntity],
        source: &str,
    ) -> Vec<telegram_api::MessageEntity> {
        crate::message_entity::get_input_message_entities::<()>(None, entities, source)
    }

    fn send_get_inline_bot_results_query(
        &mut self,
        _query: telegram_api::messages_getInlineBotResults,
        _dialog_id: crate::dialog_id::DialogId,
        _bot_user_id: crate::user_manager::UserId,
        _query_hash: u64,
        _promise: Promise<Option<td_api::InlineQueryResults>>,
    ) {
        // [blocked: net]
    }

    fn send_set_inline_bot_results_query(
        &mut self,
        _query: telegram_api::messages_setInlineBotResults,
        _promise: Promise<()>,
    ) {
        // [blocked: net]
    }

    fn send_set_bot_guest_chat_result_query(
        &mut self,
        _query_id: i64,
        _result: telegram_api::InputBotInlineResult,
        _promise: Promise<td_api::InlineMessageId>,
    ) {
        // [blocked: net]
    }

    fn send_save_prepared_inline_message_query(
        &mut self,
        _query: telegram_api::messages_savePreparedInlineMessage,
        _promise: Promise<td_api::PreparedInlineMessageId>,
    ) {
        // [blocked: net]
    }

    fn send_get_prepared_inline_message_query(
        &mut self,
        _query: telegram_api::messages_getPreparedInlineMessage,
        _bot_user_id: crate::user_manager::UserId,
        _query_hash: u64,
        _promise: Promise<Option<td_api::PreparedInlineMessage>>,
    ) {
        // [blocked: net]
    }

    fn send_save_prepared_keyboard_button_query(
        &mut self,
        _query: telegram_api::bots_requestWebViewButton,
        _promise: Promise<String>,
    ) {
        // [blocked: net]
    }

    fn send_get_requested_web_view_button_query(
        &mut self,
        _query: telegram_api::bots_getRequestedWebViewButton,
        _bot_user_id: crate::user_manager::UserId,
        _prepared_button_id: String,
        _promise: Promise<td_api::KeyboardButton>,
    ) {
        // [blocked: net]
    }

    fn send_request_simple_web_view_query(
        &mut self,
        _query: telegram_api::messages_requestSimpleWebView,
        _promise: Promise<td_api::WebAppUrl>,
    ) {
        // [blocked: net]
    }

    fn send_send_web_view_data_query(
        &mut self,
        _query: telegram_api::messages_sendWebViewData,
        _promise: Promise<()>,
    ) {
        // [blocked: net]
    }

    fn send_send_web_view_result_message_query(
        &mut self,
        _query: telegram_api::messages_sendWebViewResultMessage,
        _promise: Promise<td_api::InlineMessageId>,
    ) {
        // [blocked: net]
    }

    fn get_input_keyboard_button(
        &mut self,
        button: &td_api::KeyboardButton,
    ) -> Result<telegram_api::KeyboardButton, tdutils::status::Status> {
        // `KeyboardButton::get_keyboard_button(std::move(button), true)`
        // (InlineQueriesManager.cpp:814; KeyboardButton.cpp:82) — request
        // buttons are the point of savePreparedKeyboardButton; the wire
        // form is the record's `get_input_keyboard_button`
        // (KeyboardButton.cpp:181).
        let record = crate::keyboard_button::KeyboardButton::from_td_api(
            button.clone(),
            true,
            self.is_test_dc(),
        )?;
        Ok(record.get_input_keyboard_button())
    }

    fn get_input_reply_markup(
        &mut self,
        reply_markup: Option<td_api::ReplyMarkup>,
        is_bot: bool,
        switch_inline_buttons_allowed: bool,
    ) -> Result<Option<telegram_api::ReplyMarkup>, tdutils::status::Status> {
        // `get_inline_reply_markup` (ReplyMarkup.cpp:319-322) + the
        // null-safe free `get_input_reply_markup` (ReplyMarkup.cpp:495-502).
        let markup = crate::reply_markup::get_inline_reply_markup(
            reply_markup,
            is_bot,
            switch_inline_buttons_allowed,
            self.is_test_dc(),
        )?;
        let Some(markup) = markup else {
            return Ok(None);
        };
        Ok(crate::reply_markup::get_input_reply_markup(&markup))
    }

    fn get_input_rich_message(
        &mut self,
        _message: &td_api::inputMessageRichMessage,
    ) -> Result<Option<telegram_api::InputRichMessage>, tdutils::status::Status> {
        Ok(None) // [blocked: M.17]
    }

    fn get_input_bot_inline_message_media_invoice(
        &mut self,
        _invoice: &td_api::inputMessageInvoice,
        _reply_markup: Option<telegram_api::ReplyMarkup>,
    ) -> Result<Option<telegram_api::InputBotInlineMessage>, tdutils::status::Status> {
        Ok(None) // [blocked: Wave-3 payments]
    }

    fn create_inline_message_content(
        &mut self,
        _file_id: FileId,
        _inline_message: Option<telegram_api::BotInlineMessage>,
        _allowed_media_content_id: i32,
    ) -> Option<crate::inline_queries_manager::InlineMessageContent> {
        None // [blocked: M.9]
    }

    fn register_thumbnail(
        &mut self,
        _web_document: telegram_api::WebDocument,
    ) -> Option<td_api::Thumbnail> {
        None // [blocked: Files]
    }

    fn get_web_document_temp_photo_size(
        &mut self,
        _web_document: telegram_api::WebDocument,
    ) -> Option<crate::photo::PhotoSize> {
        None // [blocked: Files]
    }

    fn get_web_document_thumbnail_photo_size(
        &mut self,
        _web_document: telegram_api::WebDocument,
    ) -> Option<crate::photo::PhotoSize> {
        None // [blocked: Files]
    }

    fn get_theme_parameters_json_string(&self, _theme: &td_api::ThemeParameters) -> Option<String> {
        None // [blocked: Wave-3 ThemeManager]
    }

    fn on_dialog_used(
        &mut self,
        _category: crate::inline_queries_manager::TopDialogCategory,
        _dialog_id: crate::dialog_id::DialogId,
        _unix_time: i32,
    ) {
        // [blocked: M.27]
    }
}

impl crate::inline_message_manager::InlineMessageManagerEnv for TdInlineEnv<'_> {
    fn send_edit_inline_message_query(
        &mut self,
        _query: telegram_api::messages_editInlineBotMessage,
        _promise: Promise<()>,
    ) {
        // [blocked: net]
    }

    fn send_set_inline_game_score_query(
        &mut self,
        _query: telegram_api::messages_setInlineGameScore,
        _promise: Promise<()>,
    ) {
        // [blocked: net]
    }

    fn send_get_inline_game_high_scores_query(
        &mut self,
        _query: telegram_api::messages_getInlineGameHighScores,
        _promise: Promise<td_api::GameHighScores>,
    ) {
        // [blocked: net]
    }

    fn get_input_media_for_edit(
        &mut self,
        _input_message_content: Box<td_api::InputMessageContent>,
    ) -> Result<Option<crate::inline_message_manager::MediaEditPiece>, tdutils::status::Status>
    {
        Ok(None) // [blocked: M.9/M.10]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_manager::StoredDcOptions;
    use std::sync::Arc;
    use tdactor::{ConcurrentScheduler, create_actor};
    use tdmtproto::telegram_api;

    #[derive(Default)]
    struct CollectCallback {
        responses: Arc<parking_lot::Mutex<Vec<(u64, Object)>>>,
        errors: Arc<parking_lot::Mutex<Vec<(u64, td_api::Error)>>>,
    }

    impl TdCallback for CollectCallback {
        fn on_result(&mut self, id: u64, result: Object) {
            self.responses.lock().push((id, result));
        }
        fn on_error(&mut self, id: u64, error: td_api::Error) {
            self.errors.lock().push((id, error));
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
            .iter()
            .filter(|(response_id, _)| *response_id == id)
            .map(|(_, object)| object.clone())
            .collect()
    }

    fn error_of(callback: &CollectCallback, id: u64) -> td_api::Error {
        callback
            .errors
            .lock()
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
        let fired = callback.responses.lock().iter().any(|(id, object)| {
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
            let responses = callback.responses.lock();
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
                callback.responses.lock().iter().all(|(id, _)| *id == 0),
                "no request responses in WaitParameters"
            );
            assert!(callback.errors.lock().is_empty());
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
                let responses = callback.responses.lock();
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
            assert!(callback.errors.lock().is_empty());

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
                let responses = callback.responses.lock();
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
                let responses = callback.responses.lock();
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

        // the write family: the same local table through the aliasing
        // adapter (openChat/closeChat/viewMessages are user-gated,
        // deleteMessages is not in the C++)
        for (id, function) in [
            (106, Function::openChat(td_api::openChat { chat_id: 1 })),
            (107, Function::closeChat(td_api::closeChat { chat_id: 1 })),
            (
                108,
                Function::viewMessages(td_api::viewMessages {
                    chat_id: 1,
                    message_ids: vec![100],
                    source: None,
                    force_read: false,
                }),
            ),
            (
                109,
                Function::deleteMessages(td_api::deleteMessages {
                    chat_id: 1,
                    message_ids: vec![100],
                    revoke: false,
                }),
            ),
        ] {
            td.request(id, function);
            let error = error_of(&callback, id);
            assert_eq!(error.code, 400);
            assert_eq!(error.message, "Chat not found");
        }
        // the manager survived every take/restore
        assert!(td.messages_manager.dialogs.is_empty());

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

        let responses = callback.responses.lock();
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
        let responses = callback.responses.lock();
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
