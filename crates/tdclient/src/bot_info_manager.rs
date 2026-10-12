//! Port of `td/telegram/BotInfoManager.{h,cpp}` (Wave 3, M.45 — Bots I):
//! the manager of the bot-administration queries (`createBot`,
//! `getManagedBot*`, the bot name/description/about fan-in, the bot media
//! previews and the custom verification), plus the
//! [`BotInfoManagerEnv`] seam surface the sibling bot modules
//! (`BotCommand.cpp`/`BotMenuButton.cpp`/`BotQueries.cpp`) share.
//!
//! The C++ manager is an actor; the state that survives this slice
//! ([`BotInfoManager`]) is held by `Td`, and the flows take the env per
//! call (the `ThemeManagerEnv` convention, M.41).
//!
//! Seams:
//! - Every `Td::ResultHandler` send is a [`BotInfoManagerEnv`] query seam —
//!   `tdutils::log_info!` of the query name + the promise parked in the
//!   manager (`[blocked: net]`, the M.31 SetBotCommandsQuery
//!   convention; the former GetUniqueGiftChatThemesQuery example is
//!   net-live). The C++ keeps the free-function families' promises
//!   (BotCommand.cpp/BotMenuButton.cpp/BotQueries.cpp handlers) in
//!   Td-owned ResultHandlers; the manager is the bot state home in the
//!   collapsed Rust Td, so those park here too — `hangup` fails them with
//!   the same `Global::request_aborted_error()` surface.
//! - `BotInfoManager::hangup`/`timeout_expired` ride the Td's single
//!   timeout slot through [`BotInfoManager::next_deadline`] (the M.13
//!   plumbing); `MAX_QUERY_DELAY` keeps the C++ 0.01 value.
//! - `SetBotInfoQuery`'s post-result reload tails
//!   (`reload_user_full`/`reload_user`) are [`BotInfoManagerEnv`]
//!   required seams: the C++ forwards the query promise to the reload, and
//!   the reload queries are [blocked: net].
//! - The bot media preview upload machinery
//!   (`PendingBotMediaPreview`/`UploadMediaCallback`/`AddPreviewMediaQuery`,
//!   BotInfoManager.h:110-123 + BotInfoManager.cpp:371-446 + 758-769) is
//!   [blocked: StoryContent]: `PendingBotMediaPreview` holds a
//!   `unique_ptr<StoryContent>` and the upload tail rides
//!   `FileManager::resume_upload`, so `add_bot_media_preview`/
//!   `edit_bot_media_preview` park after their validation halves and the
//!   `on_upload_*`/`do_add_bot_media_preview` methods land with the
//!   story track. The media-preview file-source registries
//!   (`bot_media_preview_file_source_ids_`) are real.
//! - `convert_bot_media_preview`/`get_story_content_*`
//!   (BotInfoManager.cpp:262-273) are [blocked: StoryContent] the same
//!   way: the answer halves of `GetPreviewMediasQuery`/`GetPreviewInfoQuery`
//!   reduce to that conversion and stay behind the net seam.
//! - `FileManager::get_missing_file_parts`/`delete_partial_remote_location`
//!   (AddPreviewMediaQuery::on_error, cpp:435-445) are
//!   [blocked: StoryContent] with the query.
//! - `get_fake_input_media` (BotInfoManager.cpp:1114-1134) rides the
//!   `FileView::get_full_remote_location` InputMedia conversions the file
//!   plane has not landed yet — the
//!   [`BotInfoManagerEnv::get_fake_input_media`] seam default is the
//!   C++ `nullptr` (unresolvable file), surfacing the C++ 400s.
//! - The wire flag masks the C++ names on the query constructors
//!   (`bots.setBotInfo` about=1 description=2 bot=4 name=8,
//!   `bots.getBotInfo` bot=1, `bots.setCustomVerification` bot=1
//!   custom_description=4, `bots.editAccessSettings` add_users=2,
//!   telegram_api.tl:2885-2912) are wire-time details of the send seams;
//!   only `bots.setBotInfo`'s assembly is observable pre-net and lives in
//!   [`set_bot_info_flags`].

use std::collections::HashMap;

use crate::chat_manager::AdministratorRights;
use crate::config_result::request_aborted_error;
use crate::dialog_id::DialogId;
use crate::files::{FileId, FileSourceId};
use crate::td_api;
use crate::user_manager::{AccessRights, BotData, UserId};
use tdmtproto::telegram_api;
use tdutils::promise::{Promise, PromiseCreator};
use tdutils::status::{Result, Status};

/// The `telegram_api::bots_setBotInfo` flag masks (telegram_api.tl:2885):
/// `about` = 1<<0, `description` = 1<<1, `bot` = 1<<2, `name` = 1<<3.
pub mod bots_set_bot_info_mask {
    pub const ABOUT_MASK: i32 = 1;
    pub const DESCRIPTION_MASK: i32 = 1 << 1;
    pub const BOT_MASK: i32 = 1 << 2;
    pub const NAME_MASK: i32 = 1 << 3;
}

/// `BotInfoManager::MAX_QUERY_DELAY` (BotInfoManager.h:108).
const MAX_QUERY_DELAY: f64 = 0.01;

/// The env surface of `G()`/`td_` for the bot manager and the sibling bot
/// flows. The blocked seams are the `[blocked: net]`/`[blocked: …]`
/// conventions: the query seams log the query name and own nothing — the
/// caller parks the promise in [`BotInfoManager`] first.
pub trait BotInfoManagerEnv {
    /// `td_->auth_manager_->is_authorized()`.
    fn is_authorized(&self) -> bool;

    /// `td_->auth_manager_->is_bot()`.
    fn is_bot(&self) -> bool;

    /// `G()->is_test_dc()`.
    fn is_test_dc(&self) -> bool;

    /// `G()->td_db()->get_binlog_pmc()->get`.
    fn binlog_pmc_get_raw(&mut self, key: &[u8]) -> Vec<u8>;

    /// `G()->td_db()->get_binlog_pmc()->set`.
    fn binlog_pmc_set_raw(&mut self, key: &[u8], value: &[u8]);

    /// `td_->option_manager_->get_option_integer(name)` (the C++ default 0).
    fn get_option_integer(&mut self, name: &str) -> i64;

    /// `G()->get_option_boolean(name, false)` — the always-parse-markdown
    /// flag of the description entity conversion.
    fn get_option_boolean(&mut self, name: &str) -> bool;

    /// `send_closure(G()->td(), &Td::send_update, ...)`.
    fn send_update(&mut self, update: td_api::Update);

    // ------------------------------------------------------------------
    // The UserManager seams
    // ------------------------------------------------------------------

    /// `td_->user_manager_->get_my_id()`.
    fn get_my_id(&self) -> UserId;

    /// `td_->user_manager_->get_bot_data(user_id)` — the std `Result`
    /// like the [`UserManager`](crate::user_manager::UserManager) surface.
    fn get_bot_data(&self, user_id: UserId) -> std::result::Result<BotData, Status>;

    /// `td_->user_manager_->get_input_user(user_id)` — the std `Result`
    /// like the [`UserManager`](crate::user_manager::UserManager) surface.
    fn get_input_user(
        &mut self,
        user_id: UserId,
    ) -> std::result::Result<telegram_api::InputUser, Status>;

    /// `td_->user_manager_->get_input_users_force(user_ids)`.
    fn get_input_users_force(&mut self, user_ids: &[UserId]) -> Vec<telegram_api::InputUser>;

    /// `td_->user_manager_->get_user_id_object(user_id, source)`.
    fn get_user_id_object(&mut self, user_id: UserId, source: &str) -> i64;

    /// `td_->user_manager_->get_user_ids_object(user_ids, source)`.
    fn get_user_ids_object(&mut self, user_ids: &[UserId], source: &str) -> Vec<i64>;

    /// `td_->user_manager_->get_users_object(total_count, user_ids)`.
    fn get_users_object(&mut self, total_count: i32, user_ids: &[UserId]) -> td_api::Users;

    /// `td_->user_manager_->on_get_user(user, source)`.
    fn on_get_user(&mut self, user: telegram_api::User, source: &str) -> UserId;

    /// `td_->user_manager_->invalidate_user_full(user_id)`.
    fn invalidate_user_full(&mut self, user_id: UserId);

    /// `td_->user_manager_->have_user_force(user_id, source)`.
    fn have_user_force(&mut self, user_id: UserId, source: &str) -> bool;

    /// `td_->user_manager_->reload_user_full(user_id, promise)` — the
    /// reload query is [blocked: net]; the impl owns the forwarded
    /// promise (the C++ `SetBotInfoQuery::on_result` tail).
    fn reload_user_full(&mut self, user_id: UserId, promise: Promise<()>);

    /// `td_->user_manager_->reload_user(user_id, promise)` — see
    /// [`BotInfoManagerEnv::reload_user_full`].
    fn reload_user(&mut self, user_id: UserId, promise: Promise<()>);

    /// `td_->user_manager_->on_update_bot_has_preview_medias(user_id, value)`
    /// — the bot-info cache write is [blocked: the user bot-info step];
    /// the default logs and keeps the C++ call shape.
    fn on_update_bot_has_preview_medias(&mut self, user_id: UserId, has_preview_medias: bool) {
        let _ = (user_id, has_preview_medias);
        tdutils::log_info!(
            "UserManager::on_update_bot_has_preview_medias — [blocked: the user bot-info step]"
        );
    }

    // ------------------------------------------------------------------
    // The DialogManager seams ([blocked: M.6]-documented requirements: the
    // M.6.15a free functions take `&mut Td`, which the field-split env
    // cannot reach; the td.rs impl carries the env-local mirror of the
    // stub inventory until the dialog actor lands)
    // ------------------------------------------------------------------

    /// `td_->dialog_manager_->check_dialog_access(dialog_id,
    /// allow_secret_chats, access_rights, source)`.
    fn check_dialog_access(
        &mut self,
        dialog_id: DialogId,
        allow_secret_chats: bool,
        access_rights: AccessRights,
        source: &str,
    ) -> Result<()>;

    /// `td_->dialog_manager_->have_input_peer(dialog_id, allow_secret_chats,
    /// access_rights)`.
    fn have_input_peer(
        &mut self,
        dialog_id: DialogId,
        allow_secret_chats: bool,
        access_rights: AccessRights,
    ) -> bool;

    /// `td_->dialog_manager_->get_input_peer(dialog_id, access_rights)`.
    fn get_input_peer(
        &mut self,
        dialog_id: DialogId,
        access_rights: AccessRights,
    ) -> Option<telegram_api::InputPeer>;

    /// `td_->dialog_manager_->is_broadcast_channel(dialog_id)` — the
    /// DialogManager forwarding of the C++ ChatManager read.
    fn is_broadcast_channel(&mut self, dialog_id: DialogId) -> bool;

    /// `td_->dialog_manager_->have_dialog_force(dialog_id, source)` — the
    /// `get_message_sender_dialog_id` consumer; the default is the stub
    /// inventory's `false` (unknown dialogs force-load nothing yet).
    fn have_dialog_force(&mut self, dialog_id: DialogId, source: &str) -> bool {
        let _ = (dialog_id, source);
        tdutils::log_info!("DialogManager::have_dialog_force — [blocked: M.6]");
        false
    }

    // ------------------------------------------------------------------
    // The file-plane seams
    // ------------------------------------------------------------------

    /// `td_->file_reference_manager_->create_bot_media_preview_file_source(
    /// bot_user_id)` — [blocked: the file-source step]; the default
    /// returns the invalid id (the C++ invalid `FileSourceId`).
    fn create_bot_media_preview_file_source(&mut self, bot_user_id: UserId) -> FileSourceId {
        let _ = bot_user_id;
        tdutils::log_info!(
            "FileReferenceManager::create_bot_media_preview_file_source — [blocked: the file-source step]"
        );
        FileSourceId::new(0)
    }

    /// `td_->file_reference_manager_->create_bot_media_preview_info_file_source(
    /// bot_user_id, language_code)` — see
    /// [`BotInfoManagerEnv::create_bot_media_preview_file_source`].
    fn create_bot_media_preview_info_file_source(
        &mut self,
        bot_user_id: UserId,
        language_code: &str,
    ) -> FileSourceId {
        let _ = (bot_user_id, language_code);
        tdutils::log_info!(
            "FileReferenceManager::create_bot_media_preview_info_file_source — [blocked: the file-source step]"
        );
        FileSourceId::new(0)
    }

    /// `get_fake_input_media(file_id)` (BotInfoManager.cpp:1114-1134) —
    /// `true` models the built `InputMedia`; the default is the C++
    /// `nullptr` (the InputMedia conversions are [blocked: Files]).
    fn get_fake_input_media(&mut self, file_id: FileId) -> bool {
        let _ = file_id;
        tdutils::log_info!("BotInfoManager::get_fake_input_media — [blocked: Files]");
        false
    }

    // ------------------------------------------------------------------
    // The [blocked: net] query seams. Each one is the `send` half of the
    // C++ `Td::ResultHandler` of the same name; the answers land with the
    // net step.
    // ------------------------------------------------------------------

    /// `SetBotCommandsQuery::send` (BotCommand.cpp:35-39) —
    /// `bots.setBotCommands`.
    fn send_set_bot_commands_query(
        &mut self,
        scope: telegram_api::BotCommandScope,
        language_code: &str,
        commands: Vec<telegram_api::BotCommand>,
    ) {
        let _ = (scope, language_code, commands);
        tdutils::log_info!("Send bots.setBotCommands — [blocked: net]");
    }

    /// `ResetBotCommandsQuery::send` (BotCommand.cpp:65-68) —
    /// `bots.resetBotCommands`.
    fn send_reset_bot_commands_query(
        &mut self,
        scope: telegram_api::BotCommandScope,
        language_code: &str,
    ) {
        let _ = (scope, language_code);
        tdutils::log_info!("Send bots.resetBotCommands — [blocked: net]");
    }

    /// `GetBotCommandsQuery::send` (BotCommand.cpp:92-95) —
    /// `bots.getBotCommands`.
    fn send_get_bot_commands_query(
        &mut self,
        scope: telegram_api::BotCommandScope,
        language_code: &str,
    ) {
        let _ = (scope, language_code);
        tdutils::log_info!("Send bots.getBotCommands — [blocked: net]");
    }

    /// `SetBotMenuButtonQuery::send` (BotMenuButton.cpp:31-36) —
    /// `bots.setBotMenuButton`; `None` is the C++ `inputUserEmpty`.
    fn send_set_bot_menu_button_query(
        &mut self,
        user_id: Option<telegram_api::InputUser>,
        button: telegram_api::BotMenuButton,
    ) {
        let _ = (user_id, button);
        tdutils::log_info!("Send bots.setBotMenuButton — [blocked: net]");
    }

    /// `GetBotMenuButtonQuery::send` (BotMenuButton.cpp:63-67) —
    /// `bots.getBotMenuButton`.
    fn send_get_bot_menu_button_query(&mut self, user_id: Option<telegram_api::InputUser>) {
        let _ = user_id;
        tdutils::log_info!("Send bots.getBotMenuButton — [blocked: net]");
    }

    /// `CreateBotQuery::send` (BotInfoManager.cpp:43-50) —
    /// `bots.createBot`.
    fn send_create_bot_query(
        &mut self,
        name: &str,
        username: &str,
        via_deeplink: bool,
        manager_bot_user_id: telegram_api::InputUser,
    ) {
        let _ = (name, username, via_deeplink, manager_bot_user_id);
        tdutils::log_info!("Send bots.createBot — [blocked: net]");
    }

    /// `ExportBotTokenQuery::send` (BotInfoManager.cpp:76-82) —
    /// `bots.exportBotToken`.
    fn send_export_bot_token_query(&mut self, bot_user_id: telegram_api::InputUser, revoke: bool) {
        let _ = (bot_user_id, revoke);
        tdutils::log_info!("Send bots.exportBotToken — [blocked: net]");
    }

    /// `GetAccessSettingsQuery::send` (BotInfoManager.cpp:108-114) —
    /// `bots.getAccessSettings`.
    fn send_get_access_settings_query(&mut self, bot_user_id: telegram_api::InputUser) {
        let _ = bot_user_id;
        tdutils::log_info!("Send bots.getAccessSettings — [blocked: net]");
    }

    /// `EditAccessSettingsQuery::send` (BotInfoManager.cpp:139-152) —
    /// `bots.editAccessSettings` (`add_users` = 1<<1, derived from the
    /// possibly-empty vector at wire time).
    fn send_edit_access_settings_query(
        &mut self,
        restricted: bool,
        bot_user_id: telegram_api::InputUser,
        added_users: Vec<telegram_api::InputUser>,
    ) {
        let _ = (restricted, bot_user_id, added_users);
        tdutils::log_info!("Send bots.editAccessSettings — [blocked: net]");
    }

    /// `GetAdminedBotsQuery::send` (BotInfoManager.cpp:176-178) —
    /// `bots.getAdminedBots`.
    fn send_get_admined_bots_query(&mut self) {
        tdutils::log_info!("Send bots.getAdminedBots — [blocked: net]");
    }

    /// `SetBotGroupDefaultAdminRightsQuery::send`
    /// (BotInfoManager.cpp:203-206) — `bots.setBotGroupDefaultAdminRights`.
    fn send_set_bot_group_default_admin_rights_query(
        &mut self,
        admin_rights: telegram_api::ChatAdminRights,
    ) {
        let _ = admin_rights;
        tdutils::log_info!("Send bots.setBotGroupDefaultAdminRights — [blocked: net]");
    }

    /// `SetBotBroadcastDefaultAdminRightsQuery::send`
    /// (BotInfoManager.cpp:236-239) —
    /// `bots.setBotBroadcastDefaultAdminRights`.
    fn send_set_bot_broadcast_default_admin_rights_query(
        &mut self,
        admin_rights: telegram_api::ChatAdminRights,
    ) {
        let _ = admin_rights;
        tdutils::log_info!("Send bots.setBotBroadcastDefaultAdminRights — [blocked: net]");
    }

    /// `CanBotSendMessageQuery::send` (BotInfoManager.cpp:516-523) —
    /// `bots.canSendMessage`.
    fn send_can_bot_send_message_query(&mut self, bot_user_id: telegram_api::InputUser) {
        let _ = bot_user_id;
        tdutils::log_info!("Send bots.canSendMessage — [blocked: net]");
    }

    /// `AllowBotSendMessageQuery::send` (BotInfoManager.cpp:550-557) —
    /// `bots.allowSendMessage`.
    fn send_allow_bot_send_message_query(&mut self, bot_user_id: telegram_api::InputUser) {
        let _ = bot_user_id;
        tdutils::log_info!("Send bots.allowSendMessage — [blocked: net]");
    }

    /// `GetPreviewMediasQuery::send` (BotInfoManager.cpp:284-288) —
    /// `bots.getPreviewMedias`.
    fn send_get_preview_medias_query(
        &mut self,
        bot_user_id: UserId,
        input_user: telegram_api::InputUser,
    ) {
        let _ = (bot_user_id, input_user);
        tdutils::log_info!("Send bots.getPreviewMedias — [blocked: net]");
    }

    /// `GetPreviewInfoQuery::send` (BotInfoManager.cpp:331-337) —
    /// `bots.getPreviewInfo`.
    fn send_get_preview_info_query(
        &mut self,
        bot_user_id: UserId,
        input_user: telegram_api::InputUser,
        language_code: &str,
    ) {
        let _ = (bot_user_id, input_user, language_code);
        tdutils::log_info!("Send bots.getPreviewInfo — [blocked: net]");
    }

    /// `SetBotInfoQuery::send` (BotInfoManager.cpp:606-634) —
    /// `bots.setBotInfo`; the flags bundle is [`SetBotInfoQuery`].
    fn send_set_bot_info_query(&mut self, bot_user_id: UserId, query: SetBotInfoQuery) {
        let _ = (bot_user_id, query);
        tdutils::log_info!("Send bots.setBotInfo — [blocked: net]");
    }

    /// `GetBotInfoQuery::send` (BotInfoManager.cpp:676-687) —
    /// `bots.getBotInfo` (`bot` = 1<<0).
    fn send_get_bot_info_query(
        &mut self,
        bot_user_id: UserId,
        bot: Option<telegram_api::InputUser>,
        language_code: &str,
    ) {
        let _ = (bot_user_id, bot, language_code);
        tdutils::log_info!("Send bots.getBotInfo — [blocked: net]");
    }

    /// `SetCustomVerificationQuery::send`
    /// (BotInfoManager.cpp:723-741) — `bots.setCustomVerification`
    /// (`bot` = 1<<0, `custom_description` = 1<<2).
    fn send_set_custom_verification_query(
        &mut self,
        dialog_id: DialogId,
        bot: Option<telegram_api::InputUser>,
        is_verified: bool,
        custom_description: crate::message_entity::FormattedText,
    ) {
        let _ = (dialog_id, bot, is_verified, custom_description);
        tdutils::log_info!("Send bots.setCustomVerification — [blocked: net]");
    }

    /// `SendCustomRequestQuery::send` (BotQueries.cpp:27-30) —
    /// `bots.sendCustomRequest`.
    fn send_bots_send_custom_request_query(&mut self, method: &str, parameters: &str) {
        let _ = (method, parameters);
        tdutils::log_info!("Send bots.sendCustomRequest — [blocked: net]");
    }

    /// `AnswerCustomQueryQuery::send` (BotQueries.cpp:54-57) —
    /// `bots.answerWebhookJSONQuery`.
    fn send_bots_answer_webhook_json_query(&mut self, custom_query_id: i64, data: &str) {
        let _ = (custom_query_id, data);
        tdutils::log_info!("Send bots.answerWebhookJSONQuery — [blocked: net]");
    }

    /// `SetBotUpdatesStatusQuery::send` (BotQueries.cpp:79-82) —
    /// `help.setBotUpdatesStatus`.
    fn send_help_set_bot_updates_status_query(
        &mut self,
        pending_update_count: i32,
        error_message: &str,
    ) {
        let _ = (pending_update_count, error_message);
        tdutils::log_info!("Send help.setBotUpdatesStatus — [blocked: net]");
    }
}

/// The `SetBotInfoQuery::send` argument bundle (BotInfoManager.cpp:606-607):
/// the C++ assembles the `int32 flags` from the three `set_*` bits (about =
/// 1<<0, description = 1<<1, bot = 1<<2, name = 1<<3).
#[derive(Debug, Clone, PartialEq)]
pub struct SetBotInfoQuery {
    pub language_code: String,
    pub set_name: bool,
    pub name: String,
    pub set_description: bool,
    pub description: String,
    pub set_about: bool,
    pub about: String,
    pub bot: Option<telegram_api::InputUser>,
}

/// The `SetBotInfoQuery::send` flag assembly (BotInfoManager.cpp:608-617).
pub fn set_bot_info_flags(query: &SetBotInfoQuery) -> i32 {
    let mut flags = 0;
    if query.set_name {
        flags |= bots_set_bot_info_mask::NAME_MASK;
    }
    if query.set_about {
        flags |= bots_set_bot_info_mask::ABOUT_MASK;
    }
    if query.set_description {
        flags |= bots_set_bot_info_mask::DESCRIPTION_MASK;
    }
    if query.bot.is_some() {
        flags |= bots_set_bot_info_mask::BOT_MASK;
    }
    flags
}

/// `PendingSetBotInfoQuery` (BotInfoManager.h:125-140): the fan-in of the
/// `setBotName`/`setBotInfoDescription`/`setBotInfoAbout` requests; `type_`
/// is 0 = name, 1 = description, 2 = about.
pub struct PendingSetBotInfoQuery {
    pub bot_user_id: UserId,
    pub language_code: String,
    pub type_: i32,
    pub value: String,
    pub promise: Promise<()>,
}

/// `PendingGetBotInfoQuery` (BotInfoManager.h:142-151).
pub struct PendingGetBotInfoQuery {
    pub bot_user_id: UserId,
    pub language_code: String,
    pub type_: i32,
    pub promise: Promise<String>,
}

/// The parked `GetBotInfoQuery` reply: the C++ handler owns one promise
/// vector per info type (BotInfoManager.cpp:663-713) and fills them all
/// from the one answer.
pub struct PendingGetBotInfoReply {
    pub name_promises: Vec<Promise<String>>,
    pub description_promises: Vec<Promise<String>>,
    pub about_promises: Vec<Promise<String>>,
}

/// The parked `GetPreviewMediasQuery` reply with the handler's
/// `bot_user_id_` member (BotInfoManager.cpp:275-319).
pub struct PendingGetPreviewMediasReply {
    pub bot_user_id: UserId,
    pub promise: Promise<td_api::BotMediaPreviews>,
}

/// The parked `GetPreviewInfoQuery` reply with the handler members
/// (BotInfoManager.cpp:321-369).
pub struct PendingGetPreviewInfoReply {
    pub bot_user_id: UserId,
    pub language_code: String,
    pub promise: Promise<td_api::BotMediaPreviewInfo>,
}

/// The parked `SetCustomVerificationQuery` reply with the handler's
/// `dialog_id_` member (BotInfoManager.cpp:715-756), consumed by
/// `on_get_dialog_error` at net time.
pub struct PendingSetCustomVerificationReply {
    pub dialog_id: DialogId,
    pub promise: Promise<()>,
}

/// The `MediaPreviewSource` key of `bot_media_preview_info_file_source_ids_`
/// (BotInfoManager.h:187-205).
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
struct MediaPreviewSource {
    bot_user_id: UserId,
    language_code: String,
}

/// The port of `td::BotInfoManager`.
#[derive(Default)]
pub struct BotInfoManager {
    /// `pending_set_bot_info_queries_`.
    pending_set_bot_info_queries: Vec<PendingSetBotInfoQuery>,
    /// `pending_get_bot_info_queries_`.
    pending_get_bot_info_queries: Vec<PendingGetBotInfoQuery>,
    /// `bot_media_preview_file_source_ids_`.
    bot_media_preview_file_source_ids: HashMap<UserId, FileSourceId>,
    /// `bot_media_preview_info_file_source_ids_`, keyed by the
    /// `MediaPreviewSource` pair (the C++ `UserIdHash` + string hash).
    bot_media_preview_info_file_source_ids: HashMap<MediaPreviewSource, FileSourceId>,

    // The parked query promises, one list per C++ ResultHandler family
    // (the [blocked: net] convention; hangup fails them with
    // request_aborted_error like the C++ handler teardown).
    pending_set_bot_info_replies: Vec<Promise<()>>,
    pending_get_bot_info_replies: Vec<PendingGetBotInfoReply>,
    pending_set_bot_commands_replies: Vec<Promise<()>>,
    pending_reset_bot_commands_replies: Vec<Promise<()>>,
    pending_get_bot_commands_replies: Vec<Promise<td_api::BotCommands>>,
    pending_set_menu_button_replies: Vec<Promise<()>>,
    pending_get_menu_button_replies: Vec<Promise<td_api::BotMenuButton>>,
    pending_create_bot_replies: Vec<Promise<td_api::User>>,
    pending_get_bot_token_replies: Vec<Promise<String>>,
    pending_get_access_settings_replies: Vec<Promise<td_api::BotAccessSettings>>,
    pending_edit_access_settings_replies: Vec<Promise<()>>,
    pending_get_admined_bots_replies: Vec<Promise<td_api::Users>>,
    pending_group_rights_replies: Vec<Promise<()>>,
    pending_broadcast_rights_replies: Vec<Promise<()>>,
    pending_can_send_messages_replies: Vec<Promise<()>>,
    pending_allow_send_messages_replies: Vec<Promise<()>>,
    pending_get_preview_medias_replies: Vec<PendingGetPreviewMediasReply>,
    pending_get_preview_info_replies: Vec<PendingGetPreviewInfoReply>,
    pending_add_preview_media_replies: Vec<Promise<td_api::BotMediaPreview>>,
    pending_edit_preview_media_replies: Vec<Promise<td_api::BotMediaPreview>>,
    pending_reorder_preview_media_replies: Vec<Promise<()>>,
    pending_delete_preview_media_replies: Vec<Promise<()>>,
    pending_set_custom_verification_replies: Vec<PendingSetCustomVerificationReply>,
    pending_send_custom_request_replies: Vec<Promise<td_api::CustomRequestResult>>,
    pending_answer_custom_query_replies: Vec<Promise<()>>,

    /// The armed `MAX_QUERY_DELAY` timeout (`has_timeout()` +
    /// `set_timeout_in`); 0.0 = none.
    deadline: f64,
}

impl BotInfoManager {
    /// `BotInfoManager::BotInfoManager` (BotInfoManager.cpp:771-773): the
    /// constructor only allocates the upload callback, which is
    /// [blocked: StoryContent] with the upload machinery.
    pub fn new() -> BotInfoManager {
        BotInfoManager::default()
    }

    /// `has_timeout()` — the armed-deadline probe.
    fn has_timeout(&self) -> bool {
        self.deadline != 0.0
    }

    /// The armed `MAX_QUERY_DELAY` deadline for the Td's shared timeout
    /// slot (the M.13 plumbing); `None` = the C++ `!has_timeout()`.
    pub fn next_deadline(&self) -> Option<f64> {
        if self.has_timeout() {
            Some(self.deadline)
        } else {
            None
        }
    }

    /// `BotInfoManager::create_bot` (BotInfoManager.cpp:853-857) + the
    /// `CreateBotQuery::send` input-user half (cpp:43-50).
    pub fn create_bot(
        &mut self,
        env: &mut impl BotInfoManagerEnv,
        manager_bot_user_id: UserId,
        name: &str,
        username: &str,
        via_deeplink: bool,
        mut promise: Promise<td_api::User>,
    ) {
        // TRY_RESULT_PROMISE(promise, bot_data, get_bot_data(...)) — the
        // C++ discards `bot_data`
        if let Err(status) = env.get_bot_data(manager_bot_user_id) {
            promise.set_error(status);
            return;
        }
        let input_user = match env.get_input_user(manager_bot_user_id) {
            Ok(input_user) => input_user,
            Err(status) => {
                promise.set_error(status);
                return;
            }
        };
        self.pending_create_bot_replies.push(promise);
        env.send_create_bot_query(name, username, via_deeplink, input_user);
    }

    /// `BotInfoManager::get_bot_token` (BotInfoManager.cpp:859-862) + the
    /// `ExportBotTokenQuery::send` input-user half (cpp:76-82).
    pub fn get_bot_token(
        &mut self,
        env: &mut impl BotInfoManagerEnv,
        bot_user_id: UserId,
        revoke: bool,
        mut promise: Promise<String>,
    ) {
        if let Err(status) = env.get_bot_data(bot_user_id) {
            promise.set_error(status);
            return;
        }
        let input_user = match env.get_input_user(bot_user_id) {
            Ok(input_user) => input_user,
            Err(status) => {
                promise.set_error(status);
                return;
            }
        };
        self.pending_get_bot_token_replies.push(promise);
        env.send_export_bot_token_query(input_user, revoke);
    }

    /// `BotInfoManager::get_bot_access_settings`
    /// (BotInfoManager.cpp:864-868) + `GetAccessSettingsQuery::send`.
    pub fn get_bot_access_settings(
        &mut self,
        env: &mut impl BotInfoManagerEnv,
        bot_user_id: UserId,
        mut promise: Promise<td_api::BotAccessSettings>,
    ) {
        if let Err(status) = env.get_bot_data(bot_user_id) {
            promise.set_error(status);
            return;
        }
        let input_user = match env.get_input_user(bot_user_id) {
            Ok(input_user) => input_user,
            Err(status) => {
                promise.set_error(status);
                return;
            }
        };
        self.pending_get_access_settings_replies.push(promise);
        env.send_get_access_settings_query(input_user);
    }

    /// `BotInfoManager::set_bot_access_settings`
    /// (BotInfoManager.cpp:870-876) + `EditAccessSettingsQuery::send`
    /// (cpp:139-152): the added-user input list is computed before the bot
    /// input user, exactly like the C++.
    pub fn set_bot_access_settings(
        &mut self,
        env: &mut impl BotInfoManagerEnv,
        bot_user_id: UserId,
        settings: Option<td_api::BotAccessSettings>,
        mut promise: Promise<()>,
    ) {
        if let Err(status) = env.get_bot_data(bot_user_id) {
            promise.set_error(status);
            return;
        }
        let settings = crate::bot_access_settings::BotAccessSettings::from_td_api(settings);
        let input_users = env.get_input_users_force(settings.get_added_user_ids());
        let input_user = match env.get_input_user(bot_user_id) {
            Ok(input_user) => input_user,
            Err(status) => {
                promise.set_error(status);
                return;
            }
        };
        self.pending_edit_access_settings_replies.push(promise);
        env.send_edit_access_settings_query(settings.is_restricted(), input_user, input_users);
    }

    /// `BotInfoManager::get_owned_bots` (BotInfoManager.cpp:878-880).
    pub fn get_owned_bots(
        &mut self,
        env: &mut impl BotInfoManagerEnv,
        promise: Promise<td_api::Users>,
    ) {
        self.pending_get_admined_bots_replies.push(promise);
        env.send_get_admined_bots_query();
    }

    /// `BotInfoManager::set_default_group_administrator_rights`
    /// (BotInfoManager.cpp:882-886).
    pub fn set_default_group_administrator_rights(
        &mut self,
        env: &mut impl BotInfoManagerEnv,
        administrator_rights: AdministratorRights,
        promise: Promise<()>,
    ) {
        env.invalidate_user_full(env.get_my_id());
        self.pending_group_rights_replies.push(promise);
        env.send_set_bot_group_default_admin_rights_query(
            administrator_rights.get_chat_admin_rights(),
        );
    }

    /// `BotInfoManager::set_default_channel_administrator_rights`
    /// (BotInfoManager.cpp:888-892).
    pub fn set_default_channel_administrator_rights(
        &mut self,
        env: &mut impl BotInfoManagerEnv,
        administrator_rights: AdministratorRights,
        promise: Promise<()>,
    ) {
        env.invalidate_user_full(env.get_my_id());
        self.pending_broadcast_rights_replies.push(promise);
        env.send_set_bot_broadcast_default_admin_rights_query(
            administrator_rights.get_chat_admin_rights(),
        );
    }

    /// `BotInfoManager::can_bot_send_messages`
    /// (BotInfoManager.cpp:894-896) + `CanBotSendMessageQuery::send`
    /// (cpp:516-523).
    pub fn can_bot_send_messages(
        &mut self,
        env: &mut impl BotInfoManagerEnv,
        bot_user_id: UserId,
        mut promise: Promise<()>,
    ) {
        let input_user = match env.get_input_user(bot_user_id) {
            Ok(input_user) => input_user,
            Err(status) => {
                promise.set_error(status);
                return;
            }
        };
        self.pending_can_send_messages_replies.push(promise);
        env.send_can_bot_send_message_query(input_user);
    }

    /// `BotInfoManager::allow_bot_to_send_messages`
    /// (BotInfoManager.cpp:898-900) + `AllowBotSendMessageQuery::send`
    /// (cpp:550-557).
    pub fn allow_bot_to_send_messages(
        &mut self,
        env: &mut impl BotInfoManagerEnv,
        bot_user_id: UserId,
        mut promise: Promise<()>,
    ) {
        let input_user = match env.get_input_user(bot_user_id) {
            Ok(input_user) => input_user,
            Err(status) => {
                promise.set_error(status);
                return;
            }
        };
        self.pending_allow_send_messages_replies.push(promise);
        env.send_allow_bot_send_message_query(input_user);
    }

    /// `BotInfoManager::get_bot_media_preview_file_source_id`
    /// (BotInfoManager.cpp:902-913).
    pub fn get_bot_media_preview_file_source_id(
        &mut self,
        env: &mut impl BotInfoManagerEnv,
        bot_user_id: UserId,
    ) -> FileSourceId {
        if !bot_user_id.is_valid() {
            return FileSourceId::new(0);
        }
        let source_id = self
            .bot_media_preview_file_source_ids
            .entry(bot_user_id)
            .or_default();
        if !source_id.is_valid() {
            *source_id = env.create_bot_media_preview_file_source(bot_user_id);
        }
        *source_id
    }

    /// `BotInfoManager::get_bot_media_preview_info_file_source_id`
    /// (BotInfoManager.cpp:915-928).
    pub fn get_bot_media_preview_info_file_source_id(
        &mut self,
        env: &mut impl BotInfoManagerEnv,
        bot_user_id: UserId,
        language_code: &str,
    ) -> FileSourceId {
        if !bot_user_id.is_valid() {
            return FileSourceId::new(0);
        }
        let source_id = self
            .bot_media_preview_info_file_source_ids
            .entry(MediaPreviewSource {
                bot_user_id,
                language_code: language_code.to_owned(),
            })
            .or_default();
        if !source_id.is_valid() {
            *source_id = env.create_bot_media_preview_info_file_source(bot_user_id, language_code);
        }
        *source_id
    }

    /// `BotInfoManager::get_media_preview_bot_input_user`
    /// (BotInfoManager.cpp:930-940).
    pub fn get_media_preview_bot_input_user(
        env: &mut impl BotInfoManagerEnv,
        user_id: UserId,
        can_be_edited: bool,
    ) -> Result<telegram_api::InputUser> {
        let bot_data = match env.get_bot_data(user_id) {
            Ok(bot_data) => bot_data,
            Err(status) => return Result::from_status(status),
        };
        if can_be_edited && !bot_data.can_be_edited {
            return Result::from_status(Status::error(400, "Bot must be owned"));
        }
        if !bot_data.has_main_app {
            return Result::from_status(Status::error(400, "Bot must have the main Mini App"));
        }
        match env.get_input_user(user_id) {
            Ok(input_user) => Result::Ok(input_user),
            Err(status) => Result::from_status(status),
        }
    }

    /// `BotInfoManager::validate_bot_media_preview_language_code`
    /// (BotInfoManager.cpp:942-955).
    pub fn validate_bot_media_preview_language_code(language_code: &str) -> Status {
        if language_code.is_empty() {
            return Status::ok();
        }
        let bytes = language_code.as_bytes();
        if bytes.len() < 2 || bytes[0] == b'-' || bytes[1] == b'-' {
            return Status::error(400, "Invalid language code specified");
        }
        for c in bytes {
            if !c.is_ascii_lowercase() && *c != b'-' {
                return Status::error(400, "Invalid language code specified");
            }
        }
        Status::ok()
    }

    /// `BotInfoManager::get_bot_media_previews`
    /// (BotInfoManager.cpp:957-961).
    pub fn get_bot_media_previews(
        &mut self,
        env: &mut impl BotInfoManagerEnv,
        bot_user_id: UserId,
        mut promise: Promise<td_api::BotMediaPreviews>,
    ) {
        let input_user = match Self::get_media_preview_bot_input_user(env, bot_user_id, false) {
            tdutils::status::Result::Ok(input_user) => input_user,
            tdutils::status::Result::Err(status) => {
                promise.set_error(status);
                return;
            }
        };
        self.pending_get_preview_medias_replies
            .push(PendingGetPreviewMediasReply {
                bot_user_id,
                promise,
            });
        env.send_get_preview_medias_query(bot_user_id, input_user);
    }

    /// `BotInfoManager::get_bot_media_preview_info`
    /// (BotInfoManager.cpp:963-968).
    pub fn get_bot_media_preview_info(
        &mut self,
        env: &mut impl BotInfoManagerEnv,
        bot_user_id: UserId,
        language_code: &str,
        mut promise: Promise<td_api::BotMediaPreviewInfo>,
    ) {
        let input_user = match Self::get_media_preview_bot_input_user(env, bot_user_id, true) {
            tdutils::status::Result::Ok(input_user) => input_user,
            tdutils::status::Result::Err(status) => {
                promise.set_error(status);
                return;
            }
        };
        let status = Self::validate_bot_media_preview_language_code(language_code);
        if status.is_error() {
            promise.set_error(status);
            return;
        }
        self.pending_get_preview_info_replies
            .push(PendingGetPreviewInfoReply {
                bot_user_id,
                language_code: language_code.to_owned(),
                promise,
            });
        env.send_get_preview_info_query(bot_user_id, input_user, language_code);
    }

    /// `BotInfoManager::reload_bot_media_previews`
    /// (BotInfoManager.cpp:970-980): the promise adapter drops the answer
    /// value (the C++ `PromiseCreator::lambda`).
    pub fn reload_bot_media_previews(
        &mut self,
        env: &mut impl BotInfoManagerEnv,
        bot_user_id: UserId,
        promise: Promise<()>,
    ) {
        let mut promise = promise;
        let adapted: Promise<td_api::BotMediaPreviews> =
            PromiseCreator::lambda_result(move |result: Result<td_api::BotMediaPreviews>| {
                promise.set_result(result.move_map(|_| ()));
            });
        self.get_bot_media_previews(env, bot_user_id, adapted);
    }

    /// `BotInfoManager::reload_bot_media_preview_info`
    /// (BotInfoManager.cpp:982-994).
    pub fn reload_bot_media_preview_info(
        &mut self,
        env: &mut impl BotInfoManagerEnv,
        bot_user_id: UserId,
        language_code: &str,
        promise: Promise<()>,
    ) {
        let mut promise = promise;
        let adapted: Promise<td_api::BotMediaPreviewInfo> =
            PromiseCreator::lambda_result(move |result: Result<td_api::BotMediaPreviewInfo>| {
                promise.set_result(result.move_map(|_| ()));
            });
        self.get_bot_media_preview_info(env, bot_user_id, language_code, adapted);
    }

    /// `BotInfoManager::add_bot_media_preview`
    /// (BotInfoManager.cpp:996-1012): the validation half is real; the
    /// `get_input_story_content` + upload tail is [blocked: StoryContent]
    /// and the promise parks.
    pub fn add_bot_media_preview(
        &mut self,
        env: &mut impl BotInfoManagerEnv,
        bot_user_id: UserId,
        language_code: &str,
        _input_content: Option<td_api::InputStoryContent>,
        mut promise: Promise<td_api::BotMediaPreview>,
    ) {
        let input_user = match Self::get_media_preview_bot_input_user(env, bot_user_id, true) {
            tdutils::status::Result::Ok(input_user) => input_user,
            tdutils::status::Result::Err(status) => {
                promise.set_error(status);
                return;
            }
        };
        let status = Self::validate_bot_media_preview_language_code(language_code);
        if status.is_error() {
            promise.set_error(status);
            return;
        }
        // `get_input_story_content(td_, ...)` + the
        // `do_add_bot_media_preview` upload tail (cpp:1001-1011) —
        // [blocked: StoryContent]
        tdutils::log_info!("BotInfoManager::do_add_bot_media_preview — [blocked: StoryContent]");
        self.pending_add_preview_media_replies.push(promise);
        let _ = input_user;
    }

    /// `BotInfoManager::edit_bot_media_preview`
    /// (BotInfoManager.cpp:1014-1035): the `get_fake_input_media` gate
    /// answers the C++ 400 until the Files seam resolves.
    pub fn edit_bot_media_preview(
        &mut self,
        env: &mut impl BotInfoManagerEnv,
        bot_user_id: UserId,
        language_code: &str,
        file_id: FileId,
        _input_content: Option<td_api::InputStoryContent>,
        mut promise: Promise<td_api::BotMediaPreview>,
    ) {
        let input_user = match Self::get_media_preview_bot_input_user(env, bot_user_id, true) {
            tdutils::status::Result::Ok(input_user) => input_user,
            tdutils::status::Result::Err(status) => {
                promise.set_error(status);
                return;
            }
        };
        let status = Self::validate_bot_media_preview_language_code(language_code);
        if status.is_error() {
            promise.set_error(status);
            return;
        }
        let input_media = env.get_fake_input_media(file_id);
        if !input_media {
            promise.set_error(Status::error(400, "Wrong media to edit specified"));
            return;
        }
        // the `bots.editPreviewMedia` upload tail (cpp:1024-1034) —
        // [blocked: StoryContent]
        tdutils::log_info!("BotInfoManager::do_add_bot_media_preview — [blocked: StoryContent]");
        self.pending_edit_preview_media_replies.push(promise);
        let _ = input_user;
    }

    /// `BotInfoManager::reorder_bot_media_previews`
    /// (BotInfoManager.cpp:1136-1153). The C++ text for the bad-media error
    /// is "Wrong media to delete specified" here too (the C++ copy-paste).
    pub fn reorder_bot_media_previews(
        &mut self,
        env: &mut impl BotInfoManagerEnv,
        bot_user_id: UserId,
        language_code: &str,
        file_ids: &[i32],
        mut promise: Promise<()>,
    ) {
        let input_user = match Self::get_media_preview_bot_input_user(env, bot_user_id, true) {
            tdutils::status::Result::Ok(input_user) => input_user,
            tdutils::status::Result::Err(status) => {
                promise.set_error(status);
                return;
            }
        };
        let status = Self::validate_bot_media_preview_language_code(language_code);
        if status.is_error() {
            promise.set_error(status);
            return;
        }
        let mut input_media_count = 0usize;
        for file_id in file_ids {
            let input_media = env.get_fake_input_media(FileId::new(*file_id, 0));
            if !input_media {
                promise.set_error(Status::error(400, "Wrong media to delete specified"));
                return;
            }
            input_media_count += 1;
        }
        if input_media_count == 0 {
            // cpp:1148-1150 — the empty conversion sends nothing and answers
            promise.set_value(());
            return;
        }
        // the `bots.reorderPreviewMedias` send — the InputMedia payload is
        // [blocked: Files]; the promise parks behind it
        tdutils::log_info!("Send bots.reorderPreviewMedias — [blocked: Files]");
        self.pending_reorder_preview_media_replies.push(promise);
        let _ = input_user;
    }

    /// `BotInfoManager::delete_bot_media_previews`
    /// (BotInfoManager.cpp:1155-1169).
    pub fn delete_bot_media_previews(
        &mut self,
        env: &mut impl BotInfoManagerEnv,
        bot_user_id: UserId,
        language_code: &str,
        file_ids: &[i32],
        mut promise: Promise<()>,
    ) {
        let input_user = match Self::get_media_preview_bot_input_user(env, bot_user_id, true) {
            tdutils::status::Result::Ok(input_user) => input_user,
            tdutils::status::Result::Err(status) => {
                promise.set_error(status);
                return;
            }
        };
        let status = Self::validate_bot_media_preview_language_code(language_code);
        if status.is_error() {
            promise.set_error(status);
            return;
        }
        for file_id in file_ids {
            let input_media = env.get_fake_input_media(FileId::new(*file_id, 0));
            if !input_media {
                promise.set_error(Status::error(400, "Wrong media to delete specified"));
                return;
            }
        }
        // the `bots.deletePreviewMedia` send — [blocked: Files]
        tdutils::log_info!("Send bots.deletePreviewMedia — [blocked: Files]");
        self.pending_delete_preview_media_replies.push(promise);
        let _ = input_user;
    }

    /// `BotInfoManager::add_pending_set_query`
    /// (BotInfoManager.cpp:1171-1177).
    fn add_pending_set_query(
        &mut self,
        bot_user_id: UserId,
        language_code: &str,
        type_: i32,
        value: &str,
        promise: Promise<()>,
    ) {
        self.pending_set_bot_info_queries
            .push(PendingSetBotInfoQuery {
                bot_user_id,
                language_code: language_code.to_owned(),
                type_,
                value: value.to_owned(),
                promise,
            });
        if !self.has_timeout() {
            self.deadline = tdutils::time::now() + MAX_QUERY_DELAY;
        }
    }

    /// `BotInfoManager::add_pending_get_query`
    /// (BotInfoManager.cpp:1179-1185).
    fn add_pending_get_query(
        &mut self,
        bot_user_id: UserId,
        language_code: &str,
        type_: i32,
        promise: Promise<String>,
    ) {
        self.pending_get_bot_info_queries
            .push(PendingGetBotInfoQuery {
                bot_user_id,
                language_code: language_code.to_owned(),
                type_,
                promise,
            });
        if !self.has_timeout() {
            self.deadline = tdutils::time::now() + MAX_QUERY_DELAY;
        }
    }

    /// `BotInfoManager::set_bot_name` (BotInfoManager.cpp:1187-1191).
    pub fn set_bot_name(
        &mut self,
        _env: &mut impl BotInfoManagerEnv,
        bot_user_id: UserId,
        language_code: &str,
        name: &str,
        mut promise: Promise<()>,
    ) {
        if let Err(status) = crate::misc::validate_bot_language_code(language_code) {
            promise.set_error(status);
            return;
        }
        self.add_pending_set_query(bot_user_id, language_code, 0, name, promise);
    }

    /// `BotInfoManager::get_bot_name` (BotInfoManager.cpp:1193-1196).
    pub fn get_bot_name(
        &mut self,
        _env: &mut impl BotInfoManagerEnv,
        bot_user_id: UserId,
        language_code: &str,
        mut promise: Promise<String>,
    ) {
        if let Err(status) = crate::misc::validate_bot_language_code(language_code) {
            promise.set_error(status);
            return;
        }
        self.add_pending_get_query(bot_user_id, language_code, 0, promise);
    }

    /// `BotInfoManager::set_bot_info_description`
    /// (BotInfoManager.cpp:1198-1202).
    pub fn set_bot_info_description(
        &mut self,
        _env: &mut impl BotInfoManagerEnv,
        bot_user_id: UserId,
        language_code: &str,
        description: &str,
        mut promise: Promise<()>,
    ) {
        if let Err(status) = crate::misc::validate_bot_language_code(language_code) {
            promise.set_error(status);
            return;
        }
        self.add_pending_set_query(bot_user_id, language_code, 1, description, promise);
    }

    /// `BotInfoManager::get_bot_info_description`
    /// (BotInfoManager.cpp:1204-1208).
    pub fn get_bot_info_description(
        &mut self,
        _env: &mut impl BotInfoManagerEnv,
        bot_user_id: UserId,
        language_code: &str,
        mut promise: Promise<String>,
    ) {
        if let Err(status) = crate::misc::validate_bot_language_code(language_code) {
            promise.set_error(status);
            return;
        }
        self.add_pending_get_query(bot_user_id, language_code, 1, promise);
    }

    /// `BotInfoManager::set_bot_info_about`
    /// (BotInfoManager.cpp:1210-1214).
    pub fn set_bot_info_about(
        &mut self,
        _env: &mut impl BotInfoManagerEnv,
        bot_user_id: UserId,
        language_code: &str,
        about: &str,
        mut promise: Promise<()>,
    ) {
        if let Err(status) = crate::misc::validate_bot_language_code(language_code) {
            promise.set_error(status);
            return;
        }
        self.add_pending_set_query(bot_user_id, language_code, 2, about, promise);
    }

    /// `BotInfoManager::get_bot_info_about`
    /// (BotInfoManager.cpp:1216-1219).
    pub fn get_bot_info_about(
        &mut self,
        _env: &mut impl BotInfoManagerEnv,
        bot_user_id: UserId,
        language_code: &str,
        mut promise: Promise<String>,
    ) {
        if let Err(status) = crate::misc::validate_bot_language_code(language_code) {
            promise.set_error(status);
            return;
        }
        self.add_pending_get_query(bot_user_id, language_code, 2, promise);
    }

    /// `BotInfoManager::set_custom_bot_verification`
    /// (BotInfoManager.cpp:1221-1236).
    pub fn set_custom_bot_verification(
        &mut self,
        env: &mut impl BotInfoManagerEnv,
        bot_user_id: UserId,
        dialog_id: DialogId,
        is_verified: bool,
        custom_description: Option<td_api::FormattedText>,
        mut promise: Promise<()>,
    ) {
        let mut bot_input_user: Option<telegram_api::InputUser> = None;
        if bot_user_id != UserId::default() {
            // TRY_RESULT_PROMISE_ASSIGN(promise, bot_input_user, ...)
            bot_input_user = match env.get_input_user(bot_user_id) {
                Ok(input_user) => Some(input_user),
                Err(status) => {
                    promise.set_error(status);
                    return;
                }
            };
        }
        let access_rights = if is_verified {
            AccessRights::Read
        } else {
            AccessRights::Know
        };
        if !env.have_input_peer(dialog_id, false, access_rights) {
            promise.set_error(Status::error(400, "Can't access the verified entity"));
            return;
        }
        // get_formatted_text(td, get_my_dialog_id(), ...)
        // (BotInfoManager.cpp:1231-1233).
        let my_dialog_id = crate::message_entity::DialogId::User(env.get_my_id());
        let description = match crate::message_entity::get_formatted_text_of_input::<()>(
            None,
            my_dialog_id,
            custom_description,
            env.is_bot(),
            true,
            true,
            true,
            None,
            env.get_option_boolean("always_parse_markdown"),
            env.get_option_integer("session_count").try_into().unwrap(),
            None,
            false,
        ) {
            Ok(mut description) => {
                // aa9d73d18: only TextUrl entities survive in the
                // verification description.
                crate::message_entity::keep_only_text_url(&mut description);
                description
            }
            Err(error) => return promise.set_error(error),
        };
        self.pending_set_custom_verification_replies
            .push(PendingSetCustomVerificationReply { dialog_id, promise });
        env.send_set_custom_verification_query(dialog_id, bot_input_user, is_verified, description);
    }

    /// `get_bot_input_user` (BotInfoManager.cpp:575-588): `Ok(None)` is the
    /// C++ `nullptr` (the bot's own info edit).
    pub fn get_bot_input_user(
        env: &mut impl BotInfoManagerEnv,
        bot_user_id: UserId,
    ) -> Result<Option<telegram_api::InputUser>> {
        if env.is_bot() {
            if bot_user_id != UserId::default() && bot_user_id != env.get_my_id() {
                return Result::from_status(Status::error(
                    400,
                    "Invalid bot user identifier specified",
                ));
            }
        } else {
            let bot_data = match env.get_bot_data(bot_user_id) {
                Ok(bot_data) => bot_data,
                Err(status) => return Result::from_status(status),
            };
            if !bot_data.can_be_edited {
                return Result::from_status(Status::error(400, "The bot can't be edited"));
            }
            return match env.get_input_user(bot_user_id) {
                Ok(input_user) => Result::Ok(Some(input_user)),
                Err(status) => Result::from_status(status),
            };
        }
        Result::Ok(None)
    }

    /// The `SetBotInfoQuery::send` half up to the parked promise
    /// (BotInfoManager.cpp:606-634): the input-user gate, the effective
    /// bot id, the pre-send `invalidate_user_full`, and the seam.
    /// `timeout_expired` and the direct flushes share it.
    // the C++ `SetBotInfoQuery::send(bot_user_id, language_code, set_name,
    // name, set_description, description, set_about, about)` has the same
    // shape (BotInfoManager.cpp:606)
    #[allow(clippy::too_many_arguments)]
    fn dispatch_set_bot_info_query(
        &mut self,
        env: &mut impl BotInfoManagerEnv,
        bot_user_id: UserId,
        language_code: &str,
        set_name: bool,
        name: &str,
        set_description: bool,
        description: &str,
        set_about: bool,
        about: &str,
        mut promise: Promise<()>,
    ) {
        // the C++ get_bot_input_user error fires before the member sets,
        // so on_error's invalidate_bot_info() is a no-op there
        let input_user = match Self::get_bot_input_user(env, bot_user_id) {
            tdutils::status::Result::Ok(input_user) => input_user,
            tdutils::status::Result::Err(status) => {
                promise.set_error(status);
                return;
            }
        };
        let bot_user_id = if input_user.is_some() {
            bot_user_id
        } else {
            env.get_my_id()
        };
        let set_info = set_about || set_description;
        if set_info {
            env.invalidate_user_full(bot_user_id);
        }
        self.pending_set_bot_info_replies.push(promise);
        env.send_set_bot_info_query(
            bot_user_id,
            SetBotInfoQuery {
                language_code: language_code.to_owned(),
                set_name,
                name: name.to_owned(),
                set_description,
                description: description.to_owned(),
                set_about,
                about: about.to_owned(),
                bot: input_user,
            },
        );
    }

    /// The `GetBotInfoQuery::send` half up to the parked reply
    /// (BotInfoManager.cpp:676-687); the input-user gate fails all the
    /// grouped promises like the C++ `on_error`.
    fn dispatch_get_bot_info_query(
        &mut self,
        env: &mut impl BotInfoManagerEnv,
        bot_user_id: UserId,
        language_code: &str,
        name_promises: Vec<Promise<String>>,
        description_promises: Vec<Promise<String>>,
        about_promises: Vec<Promise<String>>,
    ) {
        let input_user = match Self::get_bot_input_user(env, bot_user_id) {
            tdutils::status::Result::Ok(input_user) => input_user,
            tdutils::status::Result::Err(status) => {
                let mut name_promises = name_promises;
                fail_all(&mut name_promises, status.clone());
                let mut description_promises = description_promises;
                fail_all(&mut description_promises, status.clone());
                let mut about_promises = about_promises;
                fail_all(&mut about_promises, status);
                return;
            }
        };
        self.pending_get_bot_info_replies
            .push(PendingGetBotInfoReply {
                name_promises,
                description_promises,
                about_promises,
            });
        env.send_get_bot_info_query(bot_user_id, input_user, language_code);
    }

    /// `BotInfoManager::timeout_expired` (BotInfoManager.cpp:798-851): the
    /// fan-in of the pending set/get bot info queries into one
    /// `bots.setBotInfo`/`bots.getBotInfo` per (bot, language) group.
    pub fn timeout_expired(&mut self, env: &mut impl BotInfoManagerEnv) {
        self.deadline = 0.0;
        let mut set_queries = std::mem::take(&mut self.pending_set_bot_info_queries);
        let mut get_queries = std::mem::take(&mut self.pending_get_bot_info_queries);

        set_queries.sort_by(|lhs, rhs| {
            (lhs.bot_user_id.get(), lhs.language_code.as_str())
                .cmp(&(rhs.bot_user_id.get(), rhs.language_code.as_str()))
        });
        let mut i = 0usize;
        while i < set_queries.len() {
            let mut has_value = [false; 3];
            let mut values = [String::new(), String::new(), String::new()];
            let mut promises: Vec<Promise<()>> = Vec::new();
            let mut j = i;
            while j < set_queries.len()
                && set_queries[i].bot_user_id == set_queries[j].bot_user_id
                && set_queries[i].language_code == set_queries[j].language_code
            {
                let type_index = set_queries[j].type_ as usize;
                has_value[type_index] = true;
                values[type_index] = std::mem::take(&mut set_queries[j].value);
                promises.push(std::mem::take(&mut set_queries[j].promise));
                j += 1;
            }
            // the C++ PromiseCreator::lambda distributing the query result
            // to the whole group (cpp:821-827)
            let combined: Promise<()> = PromiseCreator::lambda_result(move |result: Result<()>| {
                for promise in promises.iter_mut() {
                    promise.set_result(result.clone());
                }
            });
            self.dispatch_set_bot_info_query(
                env,
                set_queries[i].bot_user_id,
                &set_queries[i].language_code,
                has_value[0],
                &values[0],
                has_value[1],
                &values[1],
                has_value[2],
                &values[2],
                combined,
            );
            i = j;
        }

        get_queries.sort_by(|lhs, rhs| {
            (lhs.bot_user_id.get(), lhs.language_code.as_str())
                .cmp(&(rhs.bot_user_id.get(), rhs.language_code.as_str()))
        });
        let mut i = 0usize;
        while i < get_queries.len() {
            let mut promises: [Vec<Promise<String>>; 3] = [Vec::new(), Vec::new(), Vec::new()];
            let mut j = i;
            while j < get_queries.len()
                && get_queries[i].bot_user_id == get_queries[j].bot_user_id
                && get_queries[i].language_code == get_queries[j].language_code
            {
                let type_index = get_queries[j].type_ as usize;
                promises[type_index].push(std::mem::take(&mut get_queries[j].promise));
                j += 1;
            }
            let [name_promises, description_promises, about_promises] = promises;
            self.dispatch_get_bot_info_query(
                env,
                get_queries[i].bot_user_id,
                &get_queries[i].language_code,
                name_promises,
                description_promises,
                about_promises,
            );
            i = j;
        }
    }

    /// `BotInfoManager::hangup` (BotInfoManager.cpp:784-796): the pending
    /// bot-info fan-ins fail with `Global::request_aborted_error()`; the
    /// parked net tails fail the same way (the C++ ResultHandlers die with
    /// the Td's handler registry at close).
    pub fn hangup(&mut self) {
        for mut query in std::mem::take(&mut self.pending_set_bot_info_queries) {
            query.promise.set_error(request_aborted_error());
        }
        for mut query in std::mem::take(&mut self.pending_get_bot_info_queries) {
            query.promise.set_error(request_aborted_error());
        }
        for reply in std::mem::take(&mut self.pending_get_bot_info_replies) {
            let PendingGetBotInfoReply {
                mut name_promises,
                mut description_promises,
                mut about_promises,
            } = reply;
            fail_all(&mut name_promises, request_aborted_error());
            fail_all(&mut description_promises, request_aborted_error());
            fail_all(&mut about_promises, request_aborted_error());
        }
        fail_all(
            &mut self.pending_set_bot_info_replies,
            request_aborted_error(),
        );
        fail_all(
            &mut self.pending_set_bot_commands_replies,
            request_aborted_error(),
        );
        fail_all(
            &mut self.pending_reset_bot_commands_replies,
            request_aborted_error(),
        );
        fail_all(
            &mut self.pending_get_bot_commands_replies,
            request_aborted_error(),
        );
        fail_all(
            &mut self.pending_set_menu_button_replies,
            request_aborted_error(),
        );
        fail_all(
            &mut self.pending_get_menu_button_replies,
            request_aborted_error(),
        );
        fail_all(
            &mut self.pending_create_bot_replies,
            request_aborted_error(),
        );
        fail_all(
            &mut self.pending_get_bot_token_replies,
            request_aborted_error(),
        );
        fail_all(
            &mut self.pending_get_access_settings_replies,
            request_aborted_error(),
        );
        fail_all(
            &mut self.pending_edit_access_settings_replies,
            request_aborted_error(),
        );
        fail_all(
            &mut self.pending_get_admined_bots_replies,
            request_aborted_error(),
        );
        fail_all(
            &mut self.pending_group_rights_replies,
            request_aborted_error(),
        );
        fail_all(
            &mut self.pending_broadcast_rights_replies,
            request_aborted_error(),
        );
        fail_all(
            &mut self.pending_can_send_messages_replies,
            request_aborted_error(),
        );
        fail_all(
            &mut self.pending_allow_send_messages_replies,
            request_aborted_error(),
        );
        for mut reply in std::mem::take(&mut self.pending_get_preview_medias_replies) {
            reply.promise.set_error(request_aborted_error());
        }
        for mut reply in std::mem::take(&mut self.pending_get_preview_info_replies) {
            reply.promise.set_error(request_aborted_error());
        }
        fail_all(
            &mut self.pending_add_preview_media_replies,
            request_aborted_error(),
        );
        fail_all(
            &mut self.pending_edit_preview_media_replies,
            request_aborted_error(),
        );
        fail_all(
            &mut self.pending_reorder_preview_media_replies,
            request_aborted_error(),
        );
        fail_all(
            &mut self.pending_delete_preview_media_replies,
            request_aborted_error(),
        );
        for mut reply in std::mem::take(&mut self.pending_set_custom_verification_replies) {
            reply.promise.set_error(request_aborted_error());
        }
        fail_all(
            &mut self.pending_send_custom_request_replies,
            request_aborted_error(),
        );
        fail_all(
            &mut self.pending_answer_custom_query_replies,
            request_aborted_error(),
        );
    }
}

/// `fail_promises` (Promise.h): the same status to every promise.
fn fail_all<T: Send>(promises: &mut Vec<Promise<T>>, status: Status) {
    for promise in std::mem::take(promises) {
        let mut promise = promise;
        promise.set_error(status.clone());
    }
}

// The free-function families (BotCommand.cpp/BotMenuButton.cpp/
// BotQueries.cpp) park their Td-owned-handler promises here — see the
// module docs. The park entries are crate-private to the bot family.
impl BotInfoManager {
    /// The `SetBotCommandsQuery` park (BotCommand.cpp:211).
    pub(crate) fn park_set_bot_commands_query(&mut self, promise: Promise<()>) {
        self.pending_set_bot_commands_replies.push(promise);
    }

    /// The `ResetBotCommandsQuery` park (BotCommand.cpp:219).
    pub(crate) fn park_reset_bot_commands_query(&mut self, promise: Promise<()>) {
        self.pending_reset_bot_commands_replies.push(promise);
    }

    /// The `GetBotCommandsQuery` park (BotCommand.cpp:227).
    pub(crate) fn park_get_bot_commands_query(&mut self, promise: Promise<td_api::BotCommands>) {
        self.pending_get_bot_commands_replies.push(promise);
    }

    /// The `SetBotMenuButtonQuery` park (BotMenuButton.cpp:154).
    pub(crate) fn park_set_menu_button_query(&mut self, promise: Promise<()>) {
        self.pending_set_menu_button_replies.push(promise);
    }

    /// The `GetBotMenuButtonQuery` park (BotMenuButton.cpp:162).
    pub(crate) fn park_get_menu_button_query(&mut self, promise: Promise<td_api::BotMenuButton>) {
        self.pending_get_menu_button_replies.push(promise);
    }

    /// The `SendCustomRequestQuery` park (BotQueries.cpp:104).
    pub(crate) fn park_send_custom_request_query(
        &mut self,
        promise: Promise<td_api::CustomRequestResult>,
    ) {
        self.pending_send_custom_request_replies.push(promise);
    }

    /// The `AnswerCustomQueryQuery` park (BotQueries.cpp:108).
    pub(crate) fn park_answer_custom_query_query(&mut self, promise: Promise<()>) {
        self.pending_answer_custom_query_replies.push(promise);
    }
}

#[cfg(test)]
pub(crate) mod test_env {
    //! The shared recording env for the bot-family module tests: the
    //! global/options stores, the user fixture registry and the fired
    //! query seams.

    use std::collections::HashMap;

    use super::BotInfoManagerEnv;
    use crate::dialog_id::DialogId;
    use crate::files::{FileId, FileSourceId};
    use crate::global::Global;
    use crate::option_manager::OptionManager;
    use crate::td_api;
    use crate::user_manager::{AccessRights, BotData, UserEnv, UserId, UserManager};
    use tdmtproto::telegram_api;
    use tdutils::promise::Promise;
    use tdutils::status::{Result, Status};

    pub(crate) struct TestBotEnv<'a> {
        pub(crate) global: &'a mut Global,
        pub(crate) options: Option<&'a mut OptionManager>,
        pub(crate) auth_is_authorized: bool,
        pub(crate) auth_is_bot: bool,
        pub(crate) user_manager: &'a mut UserManager,
        /// my_id for the get_my_id seam (the C++ reads the manager).
        pub(crate) my_id: i64,
        /// the bot-data fixtures: user id → (can_be_edited, has_main_app).
        pub(crate) bot_data: HashMap<i64, (bool, bool)>,
        /// the users served to `on_get_user`.
        pub(crate) received_users: Vec<telegram_api::User>,
        /// the dialog ids the dialog seams answer negatively.
        pub(crate) inaccessible_dialogs: Vec<i64>,
        /// the dialog ids `is_broadcast_channel` answers `true` for.
        pub(crate) broadcast_channels: Vec<i64>,
        pub(crate) updates: Vec<td_api::Update>,
        pub(crate) queries: Vec<String>,
        /// the file ids `get_fake_input_media` answers `true` for.
        pub(crate) fake_input_media_ids: Vec<i32>,
        /// the pmc backing store (in-memory; the pmc seam is unused by the
        /// C++ manager and kept for the env-shape parity).
        pub(crate) pmc: HashMap<Vec<u8>, Vec<u8>>,
    }

    /// The standard fixture: authorized (unless overridden), the given
    /// bot mode and my_id, no bot-data fixtures.
    pub(crate) fn fixture<'a>(
        global: &'a mut Global,
        options: Option<&'a mut OptionManager>,
        user_manager: &'a mut UserManager,
        auth_is_bot: bool,
        my_id: i64,
    ) -> TestBotEnv<'a> {
        TestBotEnv {
            global,
            options,
            auth_is_authorized: true,
            auth_is_bot,
            user_manager,
            my_id,
            bot_data: HashMap::new(),
            received_users: Vec::new(),
            inaccessible_dialogs: Vec::new(),
            broadcast_channels: Vec::new(),
            updates: Vec::new(),
            queries: Vec::new(),
            fake_input_media_ids: Vec::new(),
            pmc: HashMap::new(),
        }
    }

    impl BotInfoManagerEnv for TestBotEnv<'_> {
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
            self.pmc.get(key).cloned().unwrap_or_default()
        }

        fn binlog_pmc_set_raw(&mut self, key: &[u8], value: &[u8]) {
            self.pmc.insert(key.to_vec(), value.to_vec());
        }

        fn get_option_integer(&mut self, name: &str) -> i64 {
            self.options
                .as_deref()
                .map(|options| options.get_option_integer(name, 0))
                .unwrap_or(0)
        }

        fn get_option_boolean(&mut self, name: &str) -> bool {
            self.options
                .as_deref()
                .map(|options| options.get_option_boolean(name, false))
                .unwrap_or(false)
        }

        fn send_update(&mut self, update: td_api::Update) {
            self.updates.push(update);
        }

        fn get_my_id(&self) -> UserId {
            UserId::new(self.my_id)
        }

        fn get_bot_data(&self, user_id: UserId) -> std::result::Result<BotData, Status> {
            match self.bot_data.get(&user_id.get()) {
                Some((can_be_edited, has_main_app)) => std::result::Result::Ok(BotData {
                    can_be_edited: *can_be_edited,
                    has_main_app: *has_main_app,
                    ..Default::default()
                }),
                None => std::result::Result::Err(Status::error(400, "Bot not found")),
            }
        }

        fn get_input_user(
            &mut self,
            user_id: UserId,
        ) -> std::result::Result<telegram_api::InputUser, Status> {
            let user_env = UserEnv {
                global: self.global,
                options: self
                    .options
                    .as_deref_mut()
                    .expect("tests run with the option manager"),
                auth_is_bot: self.auth_is_bot,
            };
            self.user_manager.get_input_user(&user_env, user_id)
        }

        fn get_input_users_force(&mut self, user_ids: &[UserId]) -> Vec<telegram_api::InputUser> {
            let user_env = UserEnv {
                global: self.global,
                options: self
                    .options
                    .as_deref_mut()
                    .expect("tests run with the option manager"),
                auth_is_bot: self.auth_is_bot,
            };
            self.user_manager.get_input_users_force(&user_env, user_ids)
        }

        fn get_user_id_object(&mut self, user_id: UserId, source: &str) -> i64 {
            let mut user_env = UserEnv {
                global: self.global,
                options: self
                    .options
                    .as_deref_mut()
                    .expect("tests run with the option manager"),
                auth_is_bot: self.auth_is_bot,
            };
            self.user_manager
                .get_user_id_object(&mut user_env, user_id, source)
        }

        fn get_user_ids_object(&mut self, user_ids: &[UserId], source: &str) -> Vec<i64> {
            let mut user_env = UserEnv {
                global: self.global,
                options: self
                    .options
                    .as_deref_mut()
                    .expect("tests run with the option manager"),
                auth_is_bot: self.auth_is_bot,
            };
            user_ids
                .iter()
                .map(|user_id| {
                    self.user_manager
                        .get_user_id_object(&mut user_env, *user_id, source)
                })
                .collect()
        }

        fn get_users_object(&mut self, total_count: i32, user_ids: &[UserId]) -> td_api::Users {
            let mut user_env = UserEnv {
                global: self.global,
                options: self
                    .options
                    .as_deref_mut()
                    .expect("tests run with the option manager"),
                auth_is_bot: self.auth_is_bot,
            };
            self.user_manager
                .get_users_object_of(&mut user_env, total_count, user_ids)
        }

        fn on_get_user(&mut self, user: telegram_api::User, source: &str) -> UserId {
            let _ = source;
            let user_id = match &user {
                telegram_api::User::userEmpty(empty) => empty.id,
                telegram_api::User::user(user) => user.id,
            };
            self.received_users.push(user);
            // the db-backed intake is exercised by the UserManager tests;
            // the env records and resolves the id directly
            UserId::new(user_id)
        }

        fn invalidate_user_full(&mut self, user_id: UserId) {
            let mut user_env = UserEnv {
                global: self.global,
                options: self
                    .options
                    .as_deref_mut()
                    .expect("tests run with the option manager"),
                auth_is_bot: self.auth_is_bot,
            };
            self.user_manager
                .invalidate_user_full(&mut user_env, user_id);
        }

        fn have_user_force(&mut self, user_id: UserId, source: &str) -> bool {
            let mut user_env = UserEnv {
                global: self.global,
                options: self
                    .options
                    .as_deref_mut()
                    .expect("tests run with the option manager"),
                auth_is_bot: self.auth_is_bot,
            };
            self.user_manager
                .have_user_force(&mut user_env, user_id, source)
        }

        fn reload_user_full(&mut self, user_id: UserId, _promise: Promise<()>) {
            self.queries
                .push(format!("reload_user_full:{}", user_id.get()));
            // [blocked: net] — the forwarded promise dies with the test seam
        }

        fn reload_user(&mut self, user_id: UserId, _promise: Promise<()>) {
            self.queries.push(format!("reload_user:{}", user_id.get()));
        }

        fn check_dialog_access(
            &mut self,
            dialog_id: DialogId,
            _allow_secret_chats: bool,
            _access_rights: AccessRights,
            _source: &str,
        ) -> Result<()> {
            if self.inaccessible_dialogs.contains(&dialog_id.get()) {
                return Result::from_status(Status::error(400, "Can't access the chat"));
            }
            Result::Ok(())
        }

        fn have_input_peer(
            &mut self,
            dialog_id: DialogId,
            _allow_secret_chats: bool,
            _access_rights: AccessRights,
        ) -> bool {
            !self.inaccessible_dialogs.contains(&dialog_id.get())
        }

        fn get_input_peer(
            &mut self,
            dialog_id: DialogId,
            _access_rights: AccessRights,
        ) -> Option<telegram_api::InputPeer> {
            if self.inaccessible_dialogs.contains(&dialog_id.get()) {
                None
            } else {
                Some(telegram_api::InputPeer::inputPeerEmpty(
                    telegram_api::inputPeerEmpty {},
                ))
            }
        }

        fn is_broadcast_channel(&mut self, dialog_id: DialogId) -> bool {
            self.broadcast_channels.contains(&dialog_id.get())
        }

        fn create_bot_media_preview_file_source(&mut self, bot_user_id: UserId) -> FileSourceId {
            self.queries.push(format!(
                "create_bot_media_preview_file_source:{}",
                bot_user_id.get()
            ));
            FileSourceId::new(bot_user_id.get() as i32)
        }

        fn create_bot_media_preview_info_file_source(
            &mut self,
            bot_user_id: UserId,
            language_code: &str,
        ) -> FileSourceId {
            self.queries.push(format!(
                "create_bot_media_preview_info_file_source:{}:{}",
                bot_user_id.get(),
                language_code
            ));
            FileSourceId::new(
                bot_user_id.get() as i32 + language_code.bytes().next().unwrap_or(b'?') as i32,
            )
        }

        fn get_fake_input_media(&mut self, file_id: FileId) -> bool {
            self.fake_input_media_ids.contains(&file_id.get())
        }

        // the query seams record their names so the flow tests can assert
        // the fired queries
        fn send_set_bot_commands_query(
            &mut self,
            _scope: telegram_api::BotCommandScope,
            _language_code: &str,
            _commands: Vec<telegram_api::BotCommand>,
        ) {
            self.queries
                .push("Send bots.setBotCommands — [blocked: net]".to_owned());
        }

        fn send_reset_bot_commands_query(
            &mut self,
            _scope: telegram_api::BotCommandScope,
            _language_code: &str,
        ) {
            self.queries
                .push("Send bots.resetBotCommands — [blocked: net]".to_owned());
        }

        fn send_get_bot_commands_query(
            &mut self,
            _scope: telegram_api::BotCommandScope,
            _language_code: &str,
        ) {
            self.queries
                .push("Send bots.getBotCommands — [blocked: net]".to_owned());
        }

        fn send_set_bot_menu_button_query(
            &mut self,
            _user_id: Option<telegram_api::InputUser>,
            _button: telegram_api::BotMenuButton,
        ) {
            self.queries
                .push("Send bots.setBotMenuButton — [blocked: net]".to_owned());
        }

        fn send_get_bot_menu_button_query(&mut self, _user_id: Option<telegram_api::InputUser>) {
            self.queries
                .push("Send bots.getBotMenuButton — [blocked: net]".to_owned());
        }

        fn send_create_bot_query(
            &mut self,
            _name: &str,
            _username: &str,
            _via_deeplink: bool,
            _manager_bot_user_id: telegram_api::InputUser,
        ) {
            self.queries
                .push("Send bots.createBot — [blocked: net]".to_owned());
        }

        fn send_export_bot_token_query(
            &mut self,
            _bot_user_id: telegram_api::InputUser,
            _revoke: bool,
        ) {
            self.queries
                .push("Send bots.exportBotToken — [blocked: net]".to_owned());
        }

        fn send_get_access_settings_query(&mut self, _bot_user_id: telegram_api::InputUser) {
            self.queries
                .push("Send bots.getAccessSettings — [blocked: net]".to_owned());
        }

        fn send_edit_access_settings_query(
            &mut self,
            _restricted: bool,
            _bot_user_id: telegram_api::InputUser,
            _added_users: Vec<telegram_api::InputUser>,
        ) {
            self.queries
                .push("Send bots.editAccessSettings — [blocked: net]".to_owned());
        }

        fn send_get_admined_bots_query(&mut self) {
            self.queries
                .push("Send bots.getAdminedBots — [blocked: net]".to_owned());
        }

        fn send_set_bot_group_default_admin_rights_query(
            &mut self,
            _admin_rights: telegram_api::ChatAdminRights,
        ) {
            self.queries
                .push("Send bots.setBotGroupDefaultAdminRights — [blocked: net]".to_owned());
        }

        fn send_set_bot_broadcast_default_admin_rights_query(
            &mut self,
            _admin_rights: telegram_api::ChatAdminRights,
        ) {
            self.queries
                .push("Send bots.setBotBroadcastDefaultAdminRights — [blocked: net]".to_owned());
        }

        fn send_can_bot_send_message_query(&mut self, _bot_user_id: telegram_api::InputUser) {
            self.queries
                .push("Send bots.canSendMessage — [blocked: net]".to_owned());
        }

        fn send_allow_bot_send_message_query(&mut self, _bot_user_id: telegram_api::InputUser) {
            self.queries
                .push("Send bots.allowSendMessage — [blocked: net]".to_owned());
        }

        fn send_get_preview_medias_query(
            &mut self,
            _bot_user_id: UserId,
            _input_user: telegram_api::InputUser,
        ) {
            self.queries
                .push("Send bots.getPreviewMedias — [blocked: net]".to_owned());
        }

        fn send_get_preview_info_query(
            &mut self,
            _bot_user_id: UserId,
            _input_user: telegram_api::InputUser,
            _language_code: &str,
        ) {
            self.queries
                .push("Send bots.getPreviewInfo — [blocked: net]".to_owned());
        }

        fn send_set_bot_info_query(
            &mut self,
            _bot_user_id: UserId,
            _query: super::SetBotInfoQuery,
        ) {
            self.queries
                .push("Send bots.setBotInfo — [blocked: net]".to_owned());
        }

        fn send_get_bot_info_query(
            &mut self,
            _bot_user_id: UserId,
            _bot: Option<telegram_api::InputUser>,
            _language_code: &str,
        ) {
            self.queries
                .push("Send bots.getBotInfo — [blocked: net]".to_owned());
        }

        fn send_set_custom_verification_query(
            &mut self,
            _dialog_id: DialogId,
            _bot: Option<telegram_api::InputUser>,
            _is_verified: bool,
            _custom_description: crate::message_entity::FormattedText,
        ) {
            self.queries
                .push("Send bots.setCustomVerification — [blocked: net]".to_owned());
        }

        fn send_bots_send_custom_request_query(&mut self, _method: &str, _parameters: &str) {
            self.queries
                .push("Send bots.sendCustomRequest — [blocked: net]".to_owned());
        }

        fn send_bots_answer_webhook_json_query(&mut self, _custom_query_id: i64, _data: &str) {
            self.queries
                .push("Send bots.answerWebhookJSONQuery — [blocked: net]".to_owned());
        }

        fn send_help_set_bot_updates_status_query(
            &mut self,
            _pending_update_count: i32,
            _error_message: &str,
        ) {
            self.queries
                .push("Send help.setBotUpdatesStatus — [blocked: net]".to_owned());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::global::Global;
    use crate::option_manager::OptionManager;
    use crate::user_manager::UserManager;
    use tdutils::promise::PromiseInterface;

    // The per-module promise recorder: the M.40 RecordingPromise
    // convention (set_value AND set_error are recorded).
    struct Recorded<T: Send + 'static>(std::sync::Arc<std::sync::Mutex<Option<Result<T>>>>);

    struct RecordingPromise<T: Send + 'static> {
        recorded: Recorded<T>,
    }

    impl<T: Send + 'static> PromiseInterface<T> for RecordingPromise<T> {
        fn set_value(&mut self, value: T) {
            *self.recorded.0.lock().unwrap() = Some(Result::Ok(value));
        }

        fn set_error(&mut self, error: Status) {
            *self.recorded.0.lock().unwrap() = Some(Result::Err(error));
        }
    }

    fn formatted(text: &str) -> Option<td_api::FormattedText> {
        Some(td_api::FormattedText {
            text: text.to_owned(),
            entities: Vec::new(),
        })
    }

    fn recording_promise<T: Send + 'static>() -> (Promise<T>, Recorded<T>) {
        let recorded = Recorded(std::sync::Arc::new(std::sync::Mutex::new(None)));
        (
            Promise::from_interface(RecordingPromise {
                recorded: Recorded(recorded.0.clone()),
            }),
            recorded,
        )
    }

    fn test_env<'a>(
        global: &'a mut Global,
        options: Option<&'a mut OptionManager>,
        user_manager: &'a mut UserManager,
        is_bot: bool,
        my_id: i64,
        bot_data: Vec<(i64, bool, bool)>,
    ) -> test_env::TestBotEnv<'a> {
        test_env::TestBotEnv {
            global,
            options,
            auth_is_authorized: true,
            auth_is_bot: is_bot,
            user_manager,
            my_id,
            bot_data: bot_data
                .into_iter()
                .map(|(id, can_be_edited, has_main_app)| (id, (can_be_edited, has_main_app)))
                .collect(),
            received_users: Vec::new(),
            inaccessible_dialogs: Vec::new(),
            broadcast_channels: Vec::new(),
            updates: Vec::new(),
            queries: Vec::new(),
            fake_input_media_ids: Vec::new(),
            pmc: HashMap::new(),
        }
    }

    // --- the pure helpers ---

    #[test]
    fn set_bot_info_flags_assembly() {
        // BotInfoManager.cpp:608-617: name = 1<<3, about = 1, description
        // = 1<<1, bot = 1<<2
        let query = SetBotInfoQuery {
            language_code: String::new(),
            set_name: true,
            name: String::new(),
            set_description: false,
            description: String::new(),
            set_about: false,
            about: String::new(),
            bot: None,
        };
        assert_eq!(set_bot_info_flags(&query), 1 << 3);
        let query = SetBotInfoQuery {
            set_about: true,
            bot: Some(telegram_api::InputUser::inputUserSelf(
                telegram_api::inputUserSelf {},
            )),
            ..query
        };
        assert_eq!(set_bot_info_flags(&query), (1 << 3) | 1 | (1 << 2));
    }

    #[test]
    fn validate_bot_media_preview_language_code_halves() {
        // BotInfoManager.cpp:942-955
        assert!(BotInfoManager::validate_bot_media_preview_language_code("").is_ok());
        assert!(BotInfoManager::validate_bot_media_preview_language_code("en").is_ok());
        assert!(
            BotInfoManager::validate_bot_media_preview_language_code("a-b")
                .message()
                .contains("Invalid language code")
        );
        assert!(
            BotInfoManager::validate_bot_media_preview_language_code("-e")
                .message()
                .contains("Invalid language code")
        );
        assert!(
            BotInfoManager::validate_bot_media_preview_language_code("e-")
                .message()
                .contains("Invalid language code")
        );
        assert!(
            BotInfoManager::validate_bot_media_preview_language_code("e1")
                .message()
                .contains("Invalid language code")
        );
    }

    // --- the manager flows ---

    #[test]
    fn create_bot_gates_and_parks() {
        let mut global = Global::new();
        let mut options = OptionManager::new(false);
        let mut user_manager = UserManager::new();

        // the non-bot flow without the owned-bot gate fails first
        {
            let mut env = test_env(
                &mut global,
                Some(&mut options),
                &mut user_manager,
                false,
                1,
                vec![],
            );
            let mut manager = BotInfoManager::new();
            let (promise, recorded) = recording_promise();
            manager.create_bot(&mut env, UserId::new(77), "name", "user", false, promise);
            let recorded = recorded.0.lock().unwrap().clone();
            assert_eq!(recorded.unwrap().error().message(), "Bot not found");
            assert_eq!(env.queries.len(), 0);
        }

        // the bot flow with an owned manager bot parks and fires the seam
        {
            let mut env = test_env(
                &mut global,
                Some(&mut options),
                &mut user_manager,
                true,
                1,
                vec![(77, true, false)],
            );
            let mut manager = BotInfoManager::new();
            let (promise, recorded) = recording_promise();
            manager.create_bot(&mut env, UserId::new(77), "name", "user", true, promise);
            assert!(recorded.0.lock().unwrap().is_none(), "the promise parks");
            assert_eq!(env.queries, vec!["Send bots.createBot — [blocked: net]"]);
            assert_eq!(manager.pending_create_bot_replies.len(), 1);
        }
    }

    #[test]
    fn get_bot_token_and_access_settings_gates() {
        let mut global = Global::new();
        let mut options = OptionManager::new(false);
        let mut user_manager = UserManager::new();

        // get_bot_token
        {
            let mut env = test_env(
                &mut global,
                Some(&mut options),
                &mut user_manager,
                false,
                1,
                vec![],
            );
            let mut manager = BotInfoManager::new();
            let (promise, recorded) = recording_promise();
            manager.get_bot_token(&mut env, UserId::new(5), true, promise);
            let recorded = recorded.0.lock().unwrap().clone();
            assert_eq!(recorded.unwrap().error().message(), "Bot not found");

            let mut env = test_env(
                &mut global,
                Some(&mut options),
                &mut user_manager,
                true,
                1,
                vec![(5, true, false)],
            );
            let mut manager = BotInfoManager::new();
            let (promise, recorded) = recording_promise();
            manager.get_bot_token(&mut env, UserId::new(5), true, promise);
            assert!(recorded.0.lock().unwrap().is_none());
            assert_eq!(manager.pending_get_bot_token_replies.len(), 1);
        }

        // get_bot_access_settings
        {
            let mut env = test_env(
                &mut global,
                Some(&mut options),
                &mut user_manager,
                false,
                1,
                vec![],
            );
            let mut manager = BotInfoManager::new();
            let (promise, recorded) = recording_promise();
            manager.get_bot_access_settings(&mut env, UserId::new(6), promise);
            let recorded = recorded.0.lock().unwrap().clone();
            assert_eq!(recorded.unwrap().error().message(), "Bot not found");

            let mut env = test_env(
                &mut global,
                Some(&mut options),
                &mut user_manager,
                true,
                1,
                vec![(6, true, false)],
            );
            let mut manager = BotInfoManager::new();
            let (promise, recorded) = recording_promise();
            manager.get_bot_access_settings(&mut env, UserId::new(6), promise);
            assert!(recorded.0.lock().unwrap().is_none());
            assert_eq!(manager.pending_get_access_settings_replies.len(), 1);
        }
    }

    #[test]
    fn set_bot_access_settings_computes_the_inputs() {
        let mut global = Global::new();
        let mut options = OptionManager::new(false);
        let mut user_manager = UserManager::new();

        let mut env = test_env(
            &mut global,
            Some(&mut options),
            &mut user_manager,
            true,
            1,
            vec![(7, true, false)],
        );
        let mut manager = BotInfoManager::new();
        let (promise, recorded) = recording_promise();
        manager.set_bot_access_settings(
            &mut env,
            UserId::new(7),
            Some(td_api::BotAccessSettings {
                is_restricted: true,
                added_user_ids: vec![1, 0, 2],
            }),
            promise,
        );
        assert!(recorded.0.lock().unwrap().is_none());
        assert_eq!(
            env.queries,
            vec!["Send bots.editAccessSettings — [blocked: net]"]
        );
        assert_eq!(manager.pending_edit_access_settings_replies.len(), 1);
    }

    #[test]
    fn default_rights_invalidate_my_full() {
        let mut global = Global::new();
        let mut options = OptionManager::new(false);
        let mut user_manager = UserManager::new();

        let mut env = test_env(
            &mut global,
            Some(&mut options),
            &mut user_manager,
            true,
            1,
            vec![],
        );
        let mut manager = BotInfoManager::new();
        let (promise, recorded) = recording_promise();
        manager.set_default_group_administrator_rights(
            &mut env,
            crate::chat_manager::AdministratorRights::default(),
            promise,
        );
        assert!(recorded.0.lock().unwrap().is_none());
        assert_eq!(
            env.queries,
            vec!["Send bots.setBotGroupDefaultAdminRights — [blocked: net]"]
        );

        let (promise, recorded) = recording_promise();
        manager.set_default_channel_administrator_rights(
            &mut env,
            crate::chat_manager::AdministratorRights::default(),
            promise,
        );
        assert!(recorded.0.lock().unwrap().is_none());
        assert_eq!(
            env.queries.last().unwrap(),
            "Send bots.setBotBroadcastDefaultAdminRights — [blocked: net]"
        );
    }

    #[test]
    fn get_owned_bots_and_send_halves_park() {
        let mut global = Global::new();
        let mut options = OptionManager::new(false);
        let mut user_manager = UserManager::new();

        let mut env = test_env(
            &mut global,
            Some(&mut options),
            &mut user_manager,
            true,
            1,
            vec![],
        );
        let mut manager = BotInfoManager::new();
        let (promise, recorded) = recording_promise();
        manager.get_owned_bots(&mut env, promise);
        assert!(recorded.0.lock().unwrap().is_none());
        assert_eq!(manager.pending_get_admined_bots_replies.len(), 1);

        let (promise, recorded) = recording_promise();
        manager.can_bot_send_messages(&mut env, UserId::new(9), promise);
        assert!(recorded.0.lock().unwrap().is_none());
        assert_eq!(manager.pending_can_send_messages_replies.len(), 1);

        let (promise, recorded) = recording_promise();
        manager.allow_bot_to_send_messages(&mut env, UserId::new(9), promise);
        assert!(recorded.0.lock().unwrap().is_none());
        assert_eq!(manager.pending_allow_send_messages_replies.len(), 1);
    }

    #[test]
    fn media_preview_flows_gate_and_park() {
        let mut global = Global::new();
        let mut options = OptionManager::new(false);
        let mut user_manager = UserManager::new();

        // get_bot_media_previews needs a bot with the main Mini App
        {
            let mut env = test_env(
                &mut global,
                Some(&mut options),
                &mut user_manager,
                true,
                1,
                vec![(10, true, false)],
            );
            let mut manager = BotInfoManager::new();
            let (promise, recorded) = recording_promise();
            manager.get_bot_media_previews(&mut env, UserId::new(10), promise);
            let recorded = recorded.0.lock().unwrap().clone();
            assert_eq!(
                recorded.unwrap().error().message(),
                "Bot must have the main Mini App"
            );
        }

        // the owned gate fires before the main-app gate on the info flow
        // (the only can_be_edited=true caller besides add/edit)
        {
            let mut env = test_env(
                &mut global,
                Some(&mut options),
                &mut user_manager,
                true,
                1,
                vec![(10, false, true)],
            );
            let mut manager = BotInfoManager::new();
            let (promise, recorded) = recording_promise();
            manager.get_bot_media_preview_info(&mut env, UserId::new(10), "en", promise);
            let recorded = recorded.0.lock().unwrap().clone();
            assert_eq!(recorded.unwrap().error().message(), "Bot must be owned");
        }

        // the passing preview info validates the language code then parks
        {
            let mut env = test_env(
                &mut global,
                Some(&mut options),
                &mut user_manager,
                true,
                1,
                vec![(10, true, true)],
            );
            let mut manager = BotInfoManager::new();
            let (promise, recorded) = recording_promise();
            manager.get_bot_media_preview_info(&mut env, UserId::new(10), "bad lang", promise);
            let recorded = recorded.0.lock().unwrap().clone();
            assert_eq!(
                recorded.unwrap().error().message(),
                "Invalid language code specified"
            );

            let (promise, recorded) = recording_promise();
            manager.get_bot_media_preview_info(&mut env, UserId::new(10), "en", promise);
            assert!(recorded.0.lock().unwrap().is_none());
            assert_eq!(manager.pending_get_preview_info_replies.len(), 1);
            assert_eq!(
                manager.pending_get_preview_info_replies[0].language_code,
                "en"
            );
        }

        // reload adapts the promise into the same flow
        {
            let mut env = test_env(
                &mut global,
                Some(&mut options),
                &mut user_manager,
                true,
                1,
                vec![(10, true, true)],
            );
            let mut manager = BotInfoManager::new();
            let (promise, recorded) = recording_promise();
            manager.reload_bot_media_previews(&mut env, UserId::new(10), promise);
            assert!(recorded.0.lock().unwrap().is_none());
            assert_eq!(manager.pending_get_preview_medias_replies.len(), 1);
        }

        // add parks behind the story-content seam after the gates
        {
            let mut env = test_env(
                &mut global,
                Some(&mut options),
                &mut user_manager,
                true,
                1,
                vec![(10, true, true)],
            );
            let mut manager = BotInfoManager::new();
            let (promise, recorded) = recording_promise();
            manager.add_bot_media_preview(&mut env, UserId::new(10), "en", None, promise);
            assert!(recorded.0.lock().unwrap().is_none());
            assert_eq!(manager.pending_add_preview_media_replies.len(), 1);
        }

        // edit fails on the unresolvable fake input media (the C++ nullptr)
        {
            let mut env = test_env(
                &mut global,
                Some(&mut options),
                &mut user_manager,
                true,
                1,
                vec![(10, true, true)],
            );
            let mut manager = BotInfoManager::new();
            let (promise, recorded) = recording_promise();
            manager.edit_bot_media_preview(
                &mut env,
                UserId::new(10),
                "en",
                FileId::new(3, 0),
                None,
                promise,
            );
            let recorded = recorded.0.lock().unwrap().clone();
            assert_eq!(
                recorded.unwrap().error().message(),
                "Wrong media to edit specified"
            );
        }

        // reorder with no files answers immediately (cpp:1148-1150)
        {
            let mut env = test_env(
                &mut global,
                Some(&mut options),
                &mut user_manager,
                true,
                1,
                vec![(10, true, true)],
            );
            let mut manager = BotInfoManager::new();
            let (promise, recorded) = recording_promise();
            manager.reorder_bot_media_previews(&mut env, UserId::new(10), "en", &[], promise);
            let recorded = recorded.0.lock().unwrap().clone();
            assert!(recorded.unwrap().is_ok());
            assert!(manager.pending_reorder_preview_media_replies.is_empty());

            // a bad file id fails with the C++ copy-pasted delete text
            let (promise, recorded) = recording_promise();
            manager.reorder_bot_media_previews(&mut env, UserId::new(10), "en", &[4], promise);
            let recorded = recorded.0.lock().unwrap().clone();
            assert_eq!(
                recorded.unwrap().error().message(),
                "Wrong media to delete specified"
            );
        }
    }

    #[test]
    fn media_preview_file_source_ids_are_cached() {
        let mut global = Global::new();
        let mut options = OptionManager::new(false);
        let mut user_manager = UserManager::new();

        let mut env = test_env(
            &mut global,
            Some(&mut options),
            &mut user_manager,
            false,
            1,
            vec![],
        );
        let mut manager = BotInfoManager::new();

        // the invalid id short-circuits (cpp:903-905)
        assert!(
            !manager
                .get_bot_media_preview_file_source_id(&mut env, UserId::new(0))
                .is_valid()
        );

        let first = manager.get_bot_media_preview_file_source_id(&mut env, UserId::new(11));
        assert!(first.is_valid());
        assert_eq!(env.queries, vec!["create_bot_media_preview_file_source:11"]);
        // the second read comes from the cache
        let second = manager.get_bot_media_preview_file_source_id(&mut env, UserId::new(11));
        assert_eq!(first, second);
        assert_eq!(env.queries.len(), 1);

        let info_first =
            manager.get_bot_media_preview_info_file_source_id(&mut env, UserId::new(11), "en");
        assert!(info_first.is_valid());
        let info_cached =
            manager.get_bot_media_preview_info_file_source_id(&mut env, UserId::new(11), "en");
        assert_eq!(info_first, info_cached);
        let other_language =
            manager.get_bot_media_preview_info_file_source_id(&mut env, UserId::new(11), "ru");
        assert_ne!(info_first, other_language);
    }

    #[test]
    fn pending_bot_info_queries_fan_in_on_timeout() {
        let mut global = Global::new();
        let mut options = OptionManager::new(false);
        let mut user_manager = UserManager::new();

        let mut env = test_env(
            &mut global,
            Some(&mut options),
            &mut user_manager,
            true,
            1,
            vec![],
        );
        let mut manager = BotInfoManager::new();

        let (promise_a, recorded_a) = recording_promise();
        let (promise_b, recorded_b) = recording_promise();
        let (promise_c, recorded_c) = recording_promise();
        let (promise_d, recorded_d) = recording_promise();
        manager.set_bot_name(&mut env, UserId::default(), "en", "Bot", promise_a);
        manager.set_bot_info_description(&mut env, UserId::default(), "en", "Desc", promise_b);
        manager.set_bot_info_about(&mut env, UserId::default(), "en", "About", promise_c);
        // a different language groups separately
        manager.set_bot_name(&mut env, UserId::default(), "ru", "Бот", promise_d);
        assert!(manager.next_deadline().is_some());

        manager.timeout_expired(&mut env);
        assert!(manager.next_deadline().is_none());
        assert!(manager.pending_set_bot_info_queries.is_empty());
        // two groups → two parked set-bot-info replies
        assert_eq!(manager.pending_set_bot_info_replies.len(), 2);
        // every promise parked with the flush; the C++ answers at net time
        assert!(recorded_a.0.lock().unwrap().is_none());
        assert!(recorded_b.0.lock().unwrap().is_none());
        assert!(recorded_c.0.lock().unwrap().is_none());
        assert!(recorded_d.0.lock().unwrap().is_none());

        // get queries fan in by type buckets
        let (get_a, get_recorded_a) = recording_promise();
        let (get_b, get_recorded_b) = recording_promise();
        manager.get_bot_name(&mut env, UserId::default(), "en", get_a);
        manager.get_bot_info_about(&mut env, UserId::default(), "en", get_b);
        manager.timeout_expired(&mut env);
        assert_eq!(manager.pending_get_bot_info_replies.len(), 1);
        let reply = &manager.pending_get_bot_info_replies[0];
        assert_eq!(reply.name_promises.len(), 1);
        assert_eq!(reply.about_promises.len(), 1);
        assert!(reply.description_promises.is_empty());
        assert!(get_recorded_a.0.lock().unwrap().is_none());
        assert!(get_recorded_b.0.lock().unwrap().is_none());
    }

    #[test]
    fn set_bot_name_rejects_bad_language_codes() {
        let mut global = Global::new();
        let mut options = OptionManager::new(false);
        let mut user_manager = UserManager::new();

        let mut env = test_env(
            &mut global,
            Some(&mut options),
            &mut user_manager,
            true,
            1,
            vec![],
        );
        let mut manager = BotInfoManager::new();
        let (promise, recorded) = recording_promise();
        manager.set_bot_name(&mut env, UserId::default(), "e!", "Bot", promise);
        let recorded = recorded.0.lock().unwrap().clone();
        assert_eq!(
            recorded.unwrap().error().message(),
            "Invalid language code specified"
        );
        assert!(manager.pending_set_bot_info_queries.is_empty());
    }

    #[test]
    fn set_custom_bot_verification_gates() {
        let mut global = Global::new();
        let mut options = OptionManager::new(false);
        let mut user_manager = UserManager::new();

        let mut env = test_env(
            &mut global,
            Some(&mut options),
            &mut user_manager,
            false,
            1,
            vec![],
        );
        // the unknown bot user fails the input-user gate
        let mut manager = BotInfoManager::new();
        let (promise, recorded) = recording_promise();
        manager.set_custom_bot_verification(
            &mut env,
            UserId::new(31),
            DialogId::new(1),
            true,
            formatted("desc"),
            promise,
        );
        let recorded = recorded.0.lock().unwrap().clone();
        assert!(recorded.unwrap().is_error());

        // the inaccessible dialog fails the have_input_peer gate
        env.inaccessible_dialogs.push(2);
        let (promise, recorded) = recording_promise();
        manager.set_custom_bot_verification(
            &mut env,
            UserId::default(),
            DialogId::new(2),
            false,
            None,
            promise,
        );
        let recorded = recorded.0.lock().unwrap().clone();
        assert_eq!(
            recorded.unwrap().error().message(),
            "Can't access the verified entity"
        );

        // the passing verification parks with its dialog id
        let (promise, recorded) = recording_promise();
        manager.set_custom_bot_verification(
            &mut env,
            UserId::default(),
            DialogId::new(1),
            true,
            formatted("desc"),
            promise,
        );
        assert!(recorded.0.lock().unwrap().is_none());
        assert_eq!(manager.pending_set_custom_verification_replies.len(), 1);
        assert_eq!(
            manager.pending_set_custom_verification_replies[0]
                .dialog_id
                .get(),
            1
        );
    }

    #[test]
    fn hangup_fails_the_pending_queries() {
        let mut global = Global::new();
        let mut options = OptionManager::new(false);
        let mut user_manager = UserManager::new();

        let mut env = test_env(
            &mut global,
            Some(&mut options),
            &mut user_manager,
            true,
            1,
            vec![],
        );
        let mut manager = BotInfoManager::new();
        let (promise, recorded) = recording_promise();
        manager.set_bot_name(&mut env, UserId::default(), "en", "Bot", promise);
        let (get_promise, get_recorded) = recording_promise();
        manager.get_bot_name(&mut env, UserId::default(), "en", get_promise);

        manager.hangup();
        let recorded = recorded.0.lock().unwrap().clone();
        let error = recorded.unwrap().error().clone();
        assert_eq!(error.code(), 500);
        assert_eq!(error.message(), "Request aborted");
        let get_recorded = get_recorded.0.lock().unwrap().clone();
        assert_eq!(get_recorded.unwrap().error().message(), "Request aborted");
        assert!(manager.pending_set_bot_info_queries.is_empty());
        assert!(manager.pending_get_bot_info_queries.is_empty());
    }
}
