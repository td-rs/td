//! `td::BusinessManager` (BusinessManager.h/.cpp, M.48) + the
//! [`BusinessEnv`] seam every business value class reads through.
//!
//! The C++ manager is a thin router over thirteen `Td::ResultHandler`
//! query classes; here the query halves collapse into the env's
//! [blocked: net] seams with parked promises, while the local validation
//! (the connected-bot input checks, the dialog-access checks) is real.
//! The `get_business_features` static helper is ported too.

use crate::business_away_message::BusinessAwayMessage;

use crate::business_connected_bot::BusinessConnectedBot;
use crate::business_greeting_message::BusinessGreetingMessage;
use crate::business_intro::BusinessIntro;
use crate::business_work_hours::BusinessWorkHours;
use crate::dialog_id::DialogId;
use crate::files::FileId;
use crate::store_parse::Parser;
use crate::td_api;
use crate::user_manager::UserId;
use tdutils::promise::Promise;
use tdutils::status::Status;

/// The `G()`/`td_` surface the business value classes and the manager
/// read through (the C++ reads the same surfaces off its `Td *td_`
/// directly). The defaulted halves are the seams: the [blocked: net]
/// query tails park their promises, the sticker/file-view halves answer
/// their defaults until their tracks land.
pub trait BusinessEnv {
    fn is_bot(&self) -> bool;
    fn get_time_zone_offset(&mut self, time_zone_id: &str) -> i32;
    fn get_option_integer(&mut self, name: &str, default_value: i64) -> i64;

    // -- the user/dialog surfaces (the test envs override what they need;
    //    the defaults keep the C++-required calls observable) --
    fn force_create_dialog(&mut self, dialog_id: DialogId, source: &str, force: bool) {
        tdutils::log_info!(
            "force_create_dialog({}, {source}, {force}) — seam default",
            dialog_id.get()
        );
    }
    fn get_chat_id_object(&mut self, dialog_id: DialogId, source: &str) -> i64 {
        let _ = source;
        dialog_id.get()
    }
    fn get_input_users_force(
        &mut self,
        user_ids: &[UserId],
    ) -> Vec<tdmtproto::telegram_api::InputUser> {
        tdutils::log_info!(
            "get_input_users_force for {} users — seam default",
            user_ids.len()
        );
        Vec::new()
    }
    fn get_user_id_object(&mut self, user_id: UserId, source: &str) -> i64 {
        let _ = source;
        user_id.get()
    }
    fn get_input_user(
        &mut self,
        user_id: UserId,
    ) -> std::result::Result<tdmtproto::telegram_api::InputUser, Status> {
        tdutils::log_info!("get_input_user({}) — seam default", user_id.get());
        Err(Status::error(400, "User not found"))
    }

    // -- the message-entity plane (BusinessChatLink) --
    fn get_message_text(
        &mut self,
        message: String,
        entities: Option<Vec<tdmtproto::telegram_api::MessageEntity>>,
        source: &str,
    ) -> Option<td_api::FormattedText> {
        let _ = (message, entities, source);
        None
    }
    fn get_formatted_text_object(
        &mut self,
        text: &Option<td_api::FormattedText>,
        skip_bot_commands: bool,
        max_length: i32,
    ) -> Option<td_api::FormattedText> {
        let _ = (skip_bot_commands, max_length);
        text.clone()
    }

    // -- the sticker plane (BusinessIntro) —
    //    [blocked: the sticker/file-view plane] --
    fn on_get_sticker_document(
        &mut self,
        _document: Option<tdmtproto::telegram_api::Document>,
        _source: &str,
    ) -> FileId {
        tdutils::log_info!("on_get_sticker_document — [blocked: the sticker plane]");
        FileId::default()
    }
    fn get_sticker_object(&mut self, _file_id: FileId) -> Option<td_api::Sticker> {
        tdutils::log_info!("get_sticker_object — [blocked: the sticker plane]");
        None
    }
    fn intro_get_input_file_id(&mut self, _file: Option<td_api::InputFile>) -> FileId {
        tdutils::log_info!("intro_get_input_file_id — [blocked: Files]");
        FileId::default()
    }
    fn intro_get_input_document(
        &mut self,
        _file_id: FileId,
    ) -> Option<tdmtproto::telegram_api::InputDocument> {
        tdutils::log_info!("intro_get_input_document — [blocked: Files]");
        None
    }
    fn intro_get_sticker_file_ids(&mut self, _file_id: FileId) -> Vec<FileId> {
        tdutils::log_info!("intro_get_sticker_file_ids — [blocked: Files]");
        Vec::new()
    }
    fn intro_store_sticker(&mut self, _file_id: FileId, _out: &mut Vec<u8>) {
        tdutils::log_info!("intro_store_sticker — [blocked: the sticker plane]");
    }
    fn intro_parse_sticker(
        &mut self,
        _parser: &mut Parser,
    ) -> std::result::Result<FileId, &'static str> {
        tdutils::log_info!("intro_parse_sticker — [blocked: the sticker plane]");
        Ok(FileId::default())
    }

    // -- the update/query sinks (BusinessManager) --
    //    [blocked: net] --
    fn send_get_connected_bots_query(
        &mut self,
        _promise: Promise<Option<td_api::BusinessConnectedBotInfo>>,
    ) {
        tdutils::log_info!("Send account.getConnectedBots — [blocked: net]");
    }
    fn send_update_connected_bot_query(
        &mut self,
        _connected_bot: &BusinessConnectedBot,
        _input_user: Option<tdmtproto::telegram_api::InputUser>,
        _promise: Promise<()>,
    ) {
        tdutils::log_info!("Send account.updateConnectedBot — [blocked: net]");
    }
    fn send_confirm_bot_connection_query(
        &mut self,
        _input_user: tdmtproto::telegram_api::InputUser,
        _promise: Promise<()>,
    ) {
        tdutils::log_info!("Send account.confirmBotConnection — [blocked: net]");
    }
    fn send_toggle_connected_bot_paused_query(
        &mut self,
        _dialog_id: DialogId,
        _is_paused: bool,
        _promise: Promise<()>,
    ) {
        tdutils::log_info!("Send account.toggleConnectedBotPaused — [blocked: net]");
    }
    fn send_disable_peer_connected_bot_query(
        &mut self,
        _dialog_id: DialogId,
        _promise: Promise<()>,
    ) {
        tdutils::log_info!("Send account.disablePeerConnectedBot — [blocked: net]");
    }
    fn send_get_business_chat_links_query(&mut self, _promise: Promise<td_api::BusinessChatLinks>) {
        tdutils::log_info!("Send account.getBusinessChatLinks — [blocked: net]");
    }
    fn send_create_business_chat_link_query(
        &mut self,
        _link_info: Option<td_api::InputBusinessChatLink>,
        _promise: Promise<td_api::BusinessChatLink>,
    ) {
        tdutils::log_info!("Send account.createBusinessChatLink — [blocked: net]");
    }
    fn send_edit_business_chat_link_query(
        &mut self,
        _link: &str,
        _link_info: Option<td_api::InputBusinessChatLink>,
        _promise: Promise<td_api::BusinessChatLink>,
    ) {
        tdutils::log_info!("Send account.editBusinessChatLink — [blocked: net]");
    }
    fn send_delete_business_chat_link_query(&mut self, _link: &str, _promise: Promise<()>) {
        tdutils::log_info!("Send account.deleteBusinessChatLink — [blocked: net]");
    }
    fn send_resolve_business_chat_link_query(
        &mut self,
        _link: &str,
        _promise: Promise<td_api::BusinessChatLinkInfo>,
    ) {
        tdutils::log_info!("Send account.resolveBusinessChatLink — [blocked: net]");
    }
    fn send_update_business_location_query(
        &mut self,
        _location: crate::chat_manager::DialogLocation,
        _promise: Promise<()>,
    ) {
        tdutils::log_info!("Send account.updateBusinessLocation — [blocked: net]");
    }
    fn send_update_business_work_hours_query(
        &mut self,
        _work_hours: &BusinessWorkHours,
        _promise: Promise<()>,
    ) {
        tdutils::log_info!("Send account.updateBusinessWorkHours — [blocked: net]");
    }
    fn send_update_business_greeting_message_query(
        &mut self,
        _greeting_message: &BusinessGreetingMessage,
        _promise: Promise<()>,
    ) {
        tdutils::log_info!("Send account.updateBusinessGreetingMessage — [blocked: net]");
    }
    fn send_update_business_away_message_query(
        &mut self,
        _away_message: &BusinessAwayMessage,
        _promise: Promise<()>,
    ) {
        tdutils::log_info!("Send account.updateBusinessAwayMessage — [blocked: net]");
    }
    fn send_update_business_intro_query(&mut self, _intro: &BusinessIntro, _promise: Promise<()>) {
        tdutils::log_info!("Send account.updateBusinessIntro — [blocked: net]");
    }

    // -- the update outbox --
    fn send_update(&mut self, update: td_api::Update) {
        tdutils::log_info!("send_update({update:?}) — seam default");
    }

    // -- the BusinessConnectionManager query seams — [blocked: net] /
    //    [blocked: M.10 message plane] --
    fn send_business_message_query(
        &mut self,
        _connection_id: crate::business_connection_manager::BusinessConnectionId,
        _dialog_id: DialogId,
        _promise: Promise<td_api::BusinessMessage>,
    ) {
        tdutils::log_info!("Send business message — [blocked: M.10 message plane]");
    }
    fn send_business_message_album_query(
        &mut self,
        _connection_id: crate::business_connection_manager::BusinessConnectionId,
        _dialog_id: DialogId,
        _promise: Promise<td_api::BusinessMessages>,
    ) {
        tdutils::log_info!("Send business message album — [blocked: M.10 message plane]");
    }
    fn edit_business_message_query(
        &mut self,
        _connection_id: crate::business_connection_manager::BusinessConnectionId,
        _dialog_id: DialogId,
        _message_id: crate::message_id::MessageId,
        _promise: Promise<td_api::BusinessMessage>,
    ) {
        tdutils::log_info!("Edit business message — [blocked: M.10 message plane]");
    }
    fn stop_business_poll_query(
        &mut self,
        _connection_id: crate::business_connection_manager::BusinessConnectionId,
        _dialog_id: DialogId,
        _message_id: crate::message_id::MessageId,
        _promise: Promise<td_api::BusinessMessage>,
    ) {
        tdutils::log_info!("Stop business poll — [blocked: M.10 message plane]");
    }
    fn read_business_message_query(
        &mut self,
        _connection_id: crate::business_connection_manager::BusinessConnectionId,
        _dialog_id: DialogId,
        _message_id: crate::message_id::MessageId,
        _promise: Promise<()>,
    ) {
        tdutils::log_info!("Read business message — [blocked: M.10 message plane]");
    }
    fn delete_business_messages_query(
        &mut self,
        _connection_id: crate::business_connection_manager::BusinessConnectionId,
        _message_ids: Vec<crate::message_id::MessageId>,
        _promise: Promise<()>,
    ) {
        tdutils::log_info!("Delete business messages — [blocked: M.10 message plane]");
    }
    fn delete_business_story_query(
        &mut self,
        _connection_id: crate::business_connection_manager::BusinessConnectionId,
        _promise: Promise<()>,
    ) {
        tdutils::log_info!("Delete business story — [blocked: the story plane]");
    }
    fn set_business_name_query(
        &mut self,
        _connection_id: crate::business_connection_manager::BusinessConnectionId,
        _first_name: &str,
        _last_name: &str,
        _promise: Promise<()>,
    ) {
        tdutils::log_info!("Send account.updateBusinessName — [blocked: net]");
    }
    fn set_business_about_query(
        &mut self,
        _connection_id: crate::business_connection_manager::BusinessConnectionId,
        _about: &str,
        _promise: Promise<()>,
    ) {
        tdutils::log_info!("Send account.updateBusinessAbout — [blocked: net]");
    }
    fn set_business_username_query(
        &mut self,
        _connection_id: crate::business_connection_manager::BusinessConnectionId,
        _username: &str,
        _promise: Promise<()>,
    ) {
        tdutils::log_info!("Send account.updateBusinessUsername — [blocked: net]");
    }
    fn set_business_gift_settings_query(
        &mut self,
        _connection_id: crate::business_connection_manager::BusinessConnectionId,
        _promise: Promise<()>,
    ) {
        tdutils::log_info!("Send account.updateBusinessGiftSettings — [blocked: net]");
    }
    fn get_business_star_status_query(
        &mut self,
        _connection_id: crate::business_connection_manager::BusinessConnectionId,
        _promise: Promise<td_api::StarAmount>,
    ) {
        tdutils::log_info!("Send account.getBusinessStarAmount — [blocked: net]");
    }
    fn transfer_business_stars_query(
        &mut self,
        _connection_id: crate::business_connection_manager::BusinessConnectionId,
        _star_count: i64,
        _promise: Promise<()>,
    ) {
        tdutils::log_info!("Send account.transferBusinessStars — [blocked: net]");
    }

    // -- the account/messages-manager seams (the query answer halves) --
    fn on_confirm_authorization(&mut self, _is_confirm: bool, _hash: i64, _bot_user_id: UserId) {}
    fn on_update_dialog_business_bot_is_paused(&mut self, _dialog_id: DialogId, _is_paused: bool) {}
    fn on_update_dialog_business_bot_removed(&mut self, _dialog_id: DialogId) {}
    fn hide_all_business_bot_manager_bars(&mut self) {}
    fn on_update_my_user_location(&mut self, _location: crate::chat_manager::DialogLocation) {}
    fn on_update_my_user_work_hours(&mut self, _work_hours: &BusinessWorkHours) {}
    fn on_update_my_user_greeting_message(&mut self, _greeting_message: &BusinessGreetingMessage) {}
    fn on_update_my_user_away_message(&mut self, _away_message: &BusinessAwayMessage) {}
    fn on_update_my_user_intro(&mut self, _intro: &BusinessIntro) {}
    fn on_get_users(&mut self, _users: Vec<tdmtproto::telegram_api::User>, _source: &str) {}
    fn on_get_chats(&mut self, _chats: Vec<tdmtproto::telegram_api::Chat>, _source: &str) {}
}

/// `td::BusinessManager` (BusinessManager.h:27-71).
#[derive(Default)]
pub struct BusinessManager;

impl BusinessManager {
    pub fn new() -> BusinessManager {
        BusinessManager
    }

    /// `get_business_connected_bot` (BusinessManager.cpp:564-567).
    pub fn get_business_connected_bot(
        &mut self,
        env: &mut impl BusinessEnv,
        promise: Promise<Option<td_api::BusinessConnectedBotInfo>>,
    ) {
        env.send_get_connected_bots_query(promise);
    }

    /// `set_business_connected_bot` (BusinessManager.cpp:569-577).
    pub fn set_business_connected_bot(
        &mut self,
        env: &mut impl BusinessEnv,
        bot: Option<td_api::BusinessConnectedBot>,
        mut promise: Promise<()>,
    ) {
        let Some(bot) = bot else {
            promise.set_error(Status::error(400, "Bot must be non-empty"));
            return;
        };
        let connected_bot = BusinessConnectedBot::from_td_api(Some(bot));
        match env.get_input_user(connected_bot.get_user_id()) {
            Ok(input_user) => {
                env.send_update_connected_bot_query(&connected_bot, Some(input_user), promise)
            }
            Err(status) => promise.set_error(status),
        }
    }

    /// `confirm_business_connected_bot` (BusinessManager.cpp:579-583).
    pub fn confirm_business_connected_bot(
        &mut self,
        env: &mut impl BusinessEnv,
        bot_user_id: UserId,
        mut promise: Promise<()>,
    ) {
        match env.get_input_user(bot_user_id) {
            Ok(input_user) => {
                env.on_confirm_authorization(true, 0, bot_user_id);
                env.send_confirm_bot_connection_query(input_user, promise);
            }
            Err(status) => promise.set_error(status),
        }
    }

    /// `delete_business_connected_bot` (BusinessManager.cpp:585-589).
    pub fn delete_business_connected_bot(
        &mut self,
        env: &mut impl BusinessEnv,
        bot_user_id: UserId,
        mut promise: Promise<()>,
    ) {
        match env.get_input_user(bot_user_id) {
            Ok(input_user) => {
                env.on_confirm_authorization(true, 0, bot_user_id);
                env.send_update_connected_bot_query(
                    &BusinessConnectedBot::default(),
                    Some(input_user),
                    promise,
                );
            }
            Err(status) => promise.set_error(status),
        }
    }

    /// `toggle_business_connected_bot_dialog_is_paused`
    /// (BusinessManager.cpp:591-601). The dialog-access check runs in the
    /// request arm on `Td` (the master dialog plane has no instance
    /// method yet); the status is passed in.
    pub fn toggle_business_connected_bot_dialog_is_paused(
        &mut self,
        env: &mut impl BusinessEnv,
        dialog_id: DialogId,
        is_paused: bool,
        dialog_status: Status,
        mut promise: Promise<()>,
    ) {
        if dialog_status.is_error() {
            promise.set_error(dialog_status);
            return;
        }
        if dialog_id.get_type() != crate::dialog_id::DialogType::User {
            promise.set_error(Status::error(400, "The chat has no connected bot"));
            return;
        }
        env.on_update_dialog_business_bot_is_paused(dialog_id, is_paused);
        env.send_toggle_connected_bot_paused_query(dialog_id, is_paused, promise);
    }

    /// `remove_business_connected_bot_from_dialog`
    /// (BusinessManager.cpp:603-611). The dialog-access check runs in the
    /// request arm on `Td`; the status is passed in.
    pub fn remove_business_connected_bot_from_dialog(
        &mut self,
        env: &mut impl BusinessEnv,
        dialog_id: DialogId,
        dialog_status: Status,
        mut promise: Promise<()>,
    ) {
        if dialog_status.is_error() {
            promise.set_error(dialog_status);
            return;
        }
        if dialog_id.get_type() != crate::dialog_id::DialogType::User {
            promise.set_error(Status::error(400, "The chat has no connected bot"));
            return;
        }
        env.on_update_dialog_business_bot_removed(dialog_id);
        env.send_disable_peer_connected_bot_query(dialog_id, promise);
    }

    /// `get_business_chat_links` (BusinessManager.cpp:613-615).
    pub fn get_business_chat_links(
        &mut self,
        env: &mut impl BusinessEnv,
        promise: Promise<td_api::BusinessChatLinks>,
    ) {
        env.send_get_business_chat_links_query(promise);
    }

    /// `create_business_chat_link` (BusinessManager.cpp:617-621).
    pub fn create_business_chat_link(
        &mut self,
        env: &mut impl BusinessEnv,
        link_info: Option<td_api::InputBusinessChatLink>,
        promise: Promise<td_api::BusinessChatLink>,
    ) {
        env.send_create_business_chat_link_query(link_info, promise);
    }

    /// `edit_business_chat_link` (BusinessManager.cpp:623-628).
    pub fn edit_business_chat_link(
        &mut self,
        env: &mut impl BusinessEnv,
        link: &str,
        link_info: Option<td_api::InputBusinessChatLink>,
        promise: Promise<td_api::BusinessChatLink>,
    ) {
        env.send_edit_business_chat_link_query(link, link_info, promise);
    }

    /// `delete_business_chat_link` (BusinessManager.cpp:630-632).
    pub fn delete_business_chat_link(
        &mut self,
        env: &mut impl BusinessEnv,
        link: &str,
        promise: Promise<()>,
    ) {
        env.send_delete_business_chat_link_query(link, promise);
    }

    /// `get_business_chat_link_info` (BusinessManager.cpp:634-637).
    pub fn get_business_chat_link_info(
        &mut self,
        env: &mut impl BusinessEnv,
        link: &str,
        promise: Promise<td_api::BusinessChatLinkInfo>,
    ) {
        env.send_resolve_business_chat_link_query(link, promise);
    }

    /// `set_business_location` (BusinessManager.cpp:639-641).
    pub fn set_business_location(
        &mut self,
        env: &mut impl BusinessEnv,
        location: crate::chat_manager::DialogLocation,
        promise: Promise<()>,
    ) {
        env.send_update_business_location_query(location, promise);
    }

    /// `set_business_work_hours` (BusinessManager.cpp:643-645).
    pub fn set_business_work_hours(
        &mut self,
        env: &mut impl BusinessEnv,
        work_hours: BusinessWorkHours,
        promise: Promise<()>,
    ) {
        env.send_update_business_work_hours_query(&work_hours, promise);
    }

    /// `set_business_greeting_message` (BusinessManager.cpp:647-650).
    pub fn set_business_greeting_message(
        &mut self,
        env: &mut impl BusinessEnv,
        greeting_message: BusinessGreetingMessage,
        promise: Promise<()>,
    ) {
        env.send_update_business_greeting_message_query(&greeting_message, promise);
    }

    /// `set_business_away_message` (BusinessManager.cpp:652-654).
    pub fn set_business_away_message(
        &mut self,
        env: &mut impl BusinessEnv,
        away_message: BusinessAwayMessage,
        promise: Promise<()>,
    ) {
        env.send_update_business_away_message_query(&away_message, promise);
    }

    /// `set_business_intro` (BusinessManager.cpp:656-658).
    pub fn set_business_intro(
        &mut self,
        env: &mut impl BusinessEnv,
        intro: BusinessIntro,
        promise: Promise<()>,
    ) {
        env.send_update_business_intro_query(&intro, promise);
    }
}
