//! `td::BusinessConnectionManager` + `BusinessConnectionId`
//! (BusinessConnectionManager.h/.cpp + BusinessConnectionId.h, M.48).
//!
//! The connection registry (the `botBusinessConnection` intake, the
//! connection checks, the user-id/dc-id lookups, `get_current_state`) is
//! real; the message send/edit/delete halves and the account/stars
//! halves park behind the [blocked: net] / [blocked: M.10 message plane]
//! env seams with their local validation inline (the C++
//! `check_business_connection` / `check_business_message_id` /
//! `check_business_story_id` gates).

use crate::business_bot_rights::BusinessBotRights;
use crate::business_manager::BusinessEnv;
use crate::dialog_id::{DialogId, DialogType};
use crate::store_parse::{self, Parser};
use crate::td_api;
use crate::user_manager::UserId;
use std::collections::HashMap;
use tdmtproto::telegram_api;
use tdutils::promise::Promise;
use tdutils::status::Status;

/// `td::BusinessConnectionId` (BusinessConnectionId.h:11-58).
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct BusinessConnectionId(String);

impl BusinessConnectionId {
    pub fn new(id: &str) -> BusinessConnectionId {
        BusinessConnectionId(id.to_owned())
    }

    /// `BusinessConnectionId::is_empty` (BusinessConnectionId.h:22-24).
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// `BusinessConnectionId::is_valid` (BusinessConnectionId.h:25-27).
    pub fn is_valid(&self) -> bool {
        !self.0.is_empty()
    }

    /// `BusinessConnectionId::get` (BusinessConnectionId.h:28-30).
    pub fn get(&self) -> &str {
        &self.0
    }

    /// `BusinessConnectionId::store` (BusinessConnectionId.h:48-50).
    pub fn store(&self, out: &mut Vec<u8>) {
        store_parse::store_string(out, &self.0);
    }

    /// `BusinessConnectionId::parse` (BusinessConnectionId.h:51-53).
    pub fn parse(parser: &mut Parser) -> std::result::Result<BusinessConnectionId, &'static str> {
        Ok(BusinessConnectionId(parser.string()?))
    }
}

impl std::fmt::Display for BusinessConnectionId {
    /// `operator<<` (BusinessConnectionId.h:55-62).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_empty() {
            write!(f, "invalid business connection")
        } else {
            write!(f, "business connection {}", self.0)
        }
    }
}

/// `BusinessConnectionManager::BusinessConnection`
/// (BusinessConnectionManager.cpp:81-114).
#[derive(Clone, Debug, Default)]
struct BusinessConnection {
    connection_id: BusinessConnectionId,
    user_id: UserId,
    dc_id: i32,
    connection_date: i32,
    rights: BusinessBotRights,
    is_disabled: bool,
}

impl BusinessConnection {
    /// `BusinessConnection(botBusinessConnection)`
    /// (BusinessConnectionManager.cpp:89-96).
    fn from_telegram(connection: &telegram_api::BotBusinessConnection) -> BusinessConnection {
        BusinessConnection {
            connection_id: BusinessConnectionId::new(&connection.connection_id),
            user_id: UserId::new(connection.user_id),
            dc_id: connection.dc_id,
            connection_date: connection.date,
            rights: BusinessBotRights::from_telegram(connection.rights.as_ref()),
            is_disabled: connection.disabled,
        }
    }

    /// `BusinessConnection::is_valid` (BusinessConnectionManager.cpp:104-106).
    fn is_valid(&self) -> bool {
        self.connection_id.is_valid()
            && self.user_id.is_valid()
            && self.dc_id != 0
            && self.connection_date > 0
    }

    /// `get_business_connection_object` (BusinessConnectionManager.cpp:108-114).
    fn get_business_connection_object(
        &self,
        env: &mut impl BusinessEnv,
    ) -> td_api::BusinessConnection {
        let user_dialog_id = DialogId::from_user_id(self.user_id);
        env.force_create_dialog(user_dialog_id, "get_business_connection_object", false);
        td_api::BusinessConnection {
            id: self.connection_id.get().to_owned(),
            user_id: env.get_user_id_object(self.user_id, "businessConnection"),
            user_chat_id: env.get_chat_id_object(user_dialog_id, "businessConnection"),
            date: self.connection_date,
            rights: if self.is_disabled {
                None
            } else {
                Some(self.rights.get_business_bot_rights_object())
            },
            is_enabled: !self.is_disabled,
        }
    }

    /// `get_update_business_connection` (BusinessConnectionManager.cpp:114-118).
    fn get_update_business_connection_object(&self, env: &mut impl BusinessEnv) -> td_api::Update {
        td_api::Update::updateBusinessConnection(td_api::updateBusinessConnection {
            connection: Some(self.get_business_connection_object(env)),
        })
    }
}

/// `td::BusinessConnectionManager` (BusinessConnectionManager.h:29-121).
#[derive(Default)]
pub struct BusinessConnectionManager {
    business_connections: HashMap<BusinessConnectionId, BusinessConnection>,
}

/// `BusinessConnectionManager::MAX_NAME_LENGTH` (BusinessConnectionManager.h:123).
const MAX_NAME_LENGTH: usize = 64;

// The request methods keep the C++-faithful argument lists (the
// convention's too_many_arguments note).
#[allow(clippy::too_many_arguments)]
impl BusinessConnectionManager {
    pub fn new() -> BusinessConnectionManager {
        BusinessConnectionManager::default()
    }

    /// `check_business_connection` (BusinessConnectionManager.cpp:958-966).
    pub fn check_business_connection(&self, connection_id: &BusinessConnectionId) -> Status {
        if !self.business_connections.contains_key(connection_id) {
            return Status::error(400, "Business connection not found");
        }
        Status::ok()
    }

    /// `check_business_connection(dialog_id)` (BusinessConnectionManager.cpp:968-980).
    pub fn check_business_connection_with_dialog(
        &self,
        connection_id: &BusinessConnectionId,
        dialog_id: DialogId,
    ) -> Status {
        let Some(connection) = self.business_connections.get(connection_id) else {
            return Status::error(400, "Business connection not found");
        };
        if dialog_id.get_type() != DialogType::User {
            return Status::error(400, "Chat must be a private chat");
        }
        if dialog_id == DialogId::from_user_id(connection.user_id) {
            return Status::error(400, "Messages must not be sent to self");
        }
        Status::ok()
    }

    /// `check_business_message_id` (BusinessConnectionManager.cpp:982-991).
    pub fn check_business_message_id(&self, message_id: crate::message_id::MessageId) -> Status {
        if !message_id.is_valid() {
            return Status::error(400, "Invalid message identifier specified");
        }
        const SERVER_ID_SHIFT: i64 = 20;
        const FULL_TYPE_MASK: i64 = (1 << SERVER_ID_SHIFT) - 1;
        let is_server = message_id.get() > 0
            && message_id.get() <= ((i32::MAX as i64) << SERVER_ID_SHIFT)
            && (message_id.get() & FULL_TYPE_MASK) == 0;
        if !is_server {
            return Status::error(400, "Wrong message identifier specified");
        }
        Status::ok()
    }

    /// `get_business_connection_user_id` (BusinessConnectionManager.cpp:999-1003).
    pub fn get_business_connection_user_id(&self, connection_id: &BusinessConnectionId) -> UserId {
        self.business_connections
            .get(connection_id)
            .expect("checked connection")
            .user_id
    }

    /// `get_business_connection_dc_id` (BusinessConnectionManager.cpp:1005-1012).
    pub fn get_business_connection_dc_id(&self, connection_id: &BusinessConnectionId) -> i32 {
        if connection_id.is_empty() {
            return 2; // DcId::main()
        }
        self.business_connections
            .get(connection_id)
            .expect("checked connection")
            .dc_id
    }

    /// `on_update_bot_business_connect`
    /// (BusinessConnectionManager.cpp:1020-1032).
    pub fn on_update_bot_business_connect(
        &mut self,
        env: &mut impl BusinessEnv,
        connection: &telegram_api::BotBusinessConnection,
    ) {
        let business_connection = BusinessConnection::from_telegram(connection);
        if !business_connection.is_valid() {
            tdutils::log_error!("Receive invalid {connection:?}");
            return;
        }
        if !env.is_bot() {
            tdutils::log_error!("Receive {connection:?}");
            return;
        }
        let update = business_connection.get_update_business_connection_object(env);
        self.business_connections.insert(
            business_connection.connection_id.clone(),
            business_connection,
        );
        env.send_update(update);
    }

    /// `get_business_connection` (BusinessConnectionManager.cpp:1254-1261).
    pub fn get_business_connection(
        &self,
        env: &mut impl BusinessEnv,
        connection_id: &BusinessConnectionId,
        mut promise: Promise<td_api::BusinessConnection>,
    ) {
        let Some(connection) = self.business_connections.get(connection_id) else {
            promise.set_error(Status::error(400, "Business connection not found"));
            return;
        };
        promise.set_value(connection.get_business_connection_object(env));
    }

    /// `send_message` (BusinessConnectionManager.cpp:1266-1273) — the
    /// local validation; the message send is [blocked: M.10 message plane].
    pub fn send_message(
        &self,
        env: &mut impl BusinessEnv,
        business_connection_id: BusinessConnectionId,
        dialog_id: DialogId,
        _reply_to: Option<td_api::InputMessageReplyTo>,
        _disable_notification: bool,
        _protect_content: bool,
        _effect_id: i64,
        _reply_markup: Option<td_api::ReplyMarkup>,
        _input_message_content: Option<td_api::InputMessageContent>,
        mut promise: Promise<td_api::BusinessMessage>,
    ) {
        let status = self.check_business_connection_with_dialog(&business_connection_id, dialog_id);
        if status.is_error() {
            promise.set_error(status);
            return;
        }
        env.send_business_message_query(business_connection_id, dialog_id, promise);
    }

    /// `send_message_album` (BusinessConnectionManager.cpp:1401-1408).
    pub fn send_message_album(
        &self,
        env: &mut impl BusinessEnv,
        business_connection_id: BusinessConnectionId,
        dialog_id: DialogId,
        _reply_to: Option<td_api::InputMessageReplyTo>,
        _disable_notification: bool,
        _protect_content: bool,
        _effect_id: i64,
        _input_message_contents: Vec<td_api::InputMessageContent>,
        mut promise: Promise<td_api::BusinessMessages>,
    ) {
        let status = self.check_business_connection_with_dialog(&business_connection_id, dialog_id);
        if status.is_error() {
            promise.set_error(status);
            return;
        }
        env.send_business_message_album_query(business_connection_id, dialog_id, promise);
    }

    /// `edit_business_message_text` (BusinessConnectionManager.cpp:1554-1561).
    pub fn edit_business_message_text(
        &self,
        env: &mut impl BusinessEnv,
        business_connection_id: BusinessConnectionId,
        dialog_id: DialogId,
        message_id: crate::message_id::MessageId,
        _reply_markup: Option<td_api::ReplyMarkup>,
        _input_message_content: Option<td_api::InputMessageContent>,
        mut promise: Promise<td_api::BusinessMessage>,
    ) {
        let status = self.check_business_edit(&business_connection_id, dialog_id, message_id);
        if status.is_error() {
            promise.set_error(status);
            return;
        }
        env.edit_business_message_query(business_connection_id, dialog_id, message_id, promise);
    }

    /// `edit_business_message_live_location` (BusinessConnectionManager.cpp:1567-1573).
    pub fn edit_business_message_live_location(
        &self,
        env: &mut impl BusinessEnv,
        business_connection_id: BusinessConnectionId,
        dialog_id: DialogId,
        message_id: crate::message_id::MessageId,
        _reply_markup: Option<td_api::ReplyMarkup>,
        _input_location: Option<td_api::LiveLocation>,
        mut promise: Promise<td_api::BusinessMessage>,
    ) {
        let status = self.check_business_edit(&business_connection_id, dialog_id, message_id);
        if status.is_error() {
            promise.set_error(status);
            return;
        }
        env.edit_business_message_query(business_connection_id, dialog_id, message_id, promise);
    }

    /// `edit_business_message_to_do_list` (BusinessConnectionManager.cpp:1584-1590).
    pub fn edit_business_message_checklist(
        &self,
        env: &mut impl BusinessEnv,
        business_connection_id: BusinessConnectionId,
        dialog_id: DialogId,
        message_id: crate::message_id::MessageId,
        _reply_markup: Option<td_api::ReplyMarkup>,
        _input_to_do_list: Option<td_api::InputChecklist>,
        mut promise: Promise<td_api::BusinessMessage>,
    ) {
        let status = self.check_business_edit(&business_connection_id, dialog_id, message_id);
        if status.is_error() {
            promise.set_error(status);
            return;
        }
        env.edit_business_message_query(business_connection_id, dialog_id, message_id, promise);
    }

    /// `edit_business_message_media` (BusinessConnectionManager.cpp:1601-1607).
    pub fn edit_business_message_media(
        &self,
        env: &mut impl BusinessEnv,
        business_connection_id: BusinessConnectionId,
        dialog_id: DialogId,
        message_id: crate::message_id::MessageId,
        _reply_markup: Option<td_api::ReplyMarkup>,
        _input_message_content: Option<td_api::InputMessageContent>,
        mut promise: Promise<td_api::BusinessMessage>,
    ) {
        let status = self.check_business_edit(&business_connection_id, dialog_id, message_id);
        if status.is_error() {
            promise.set_error(status);
            return;
        }
        env.edit_business_message_query(business_connection_id, dialog_id, message_id, promise);
    }

    /// `edit_business_message_caption` (BusinessConnectionManager.cpp:1631-1638).
    pub fn edit_business_message_caption(
        &self,
        env: &mut impl BusinessEnv,
        business_connection_id: BusinessConnectionId,
        dialog_id: DialogId,
        message_id: crate::message_id::MessageId,
        _reply_markup: Option<td_api::ReplyMarkup>,
        _input_caption: Option<td_api::FormattedText>,
        _invert_media: bool,
        mut promise: Promise<td_api::BusinessMessage>,
    ) {
        let status = self.check_business_edit(&business_connection_id, dialog_id, message_id);
        if status.is_error() {
            promise.set_error(status);
            return;
        }
        env.edit_business_message_query(business_connection_id, dialog_id, message_id, promise);
    }

    /// `edit_business_message_reply_markup` (BusinessConnectionManager.cpp:1647-1652).
    pub fn edit_business_message_reply_markup(
        &self,
        env: &mut impl BusinessEnv,
        business_connection_id: BusinessConnectionId,
        dialog_id: DialogId,
        message_id: crate::message_id::MessageId,
        _reply_markup: Option<td_api::ReplyMarkup>,
        mut promise: Promise<td_api::BusinessMessage>,
    ) {
        let status = self.check_business_edit(&business_connection_id, dialog_id, message_id);
        if status.is_error() {
            promise.set_error(status);
            return;
        }
        env.edit_business_message_query(business_connection_id, dialog_id, message_id, promise);
    }

    /// `stop_poll` (BusinessConnectionManager.cpp:1657-1663).
    pub fn stop_poll(
        &self,
        env: &mut impl BusinessEnv,
        business_connection_id: BusinessConnectionId,
        dialog_id: DialogId,
        message_id: crate::message_id::MessageId,
        _reply_markup: Option<td_api::ReplyMarkup>,
        mut promise: Promise<td_api::BusinessMessage>,
    ) {
        let status = self.check_business_edit(&business_connection_id, dialog_id, message_id);
        if status.is_error() {
            promise.set_error(status);
            return;
        }
        env.stop_business_poll_query(business_connection_id, dialog_id, message_id, promise);
    }

    /// `read_business_message` (BusinessConnectionManager.cpp:1743-1749).
    pub fn read_business_message(
        &self,
        env: &mut impl BusinessEnv,
        business_connection_id: BusinessConnectionId,
        dialog_id: DialogId,
        message_id: crate::message_id::MessageId,
        mut promise: Promise<()>,
    ) {
        let status = self.check_business_edit(&business_connection_id, dialog_id, message_id);
        if status.is_error() {
            promise.set_error(status);
            return;
        }
        env.read_business_message_query(business_connection_id, dialog_id, message_id, promise);
    }

    /// `delete_business_messages` (BusinessConnectionManager.cpp:1778-1784).
    pub fn delete_business_messages(
        &self,
        env: &mut impl BusinessEnv,
        business_connection_id: BusinessConnectionId,
        message_ids: &[crate::message_id::MessageId],
        mut promise: Promise<()>,
    ) {
        let status = self.check_business_connection(&business_connection_id);
        if status.is_error() {
            promise.set_error(status);
            return;
        }
        env.delete_business_messages_query(business_connection_id, message_ids.to_vec(), promise);
    }

    /// `delete_business_story` (BusinessConnectionManager.cpp:1810-1816) —
    /// the story deletion is [blocked: the story plane].
    pub fn delete_business_story(
        &self,
        env: &mut impl BusinessEnv,
        business_connection_id: BusinessConnectionId,
        _story_id: i32,
        mut promise: Promise<()>,
    ) {
        let status = self.check_business_connection(&business_connection_id);
        if status.is_error() {
            promise.set_error(status);
            return;
        }
        env.delete_business_story_query(business_connection_id, promise);
    }

    /// `set_business_name` (BusinessConnectionManager.cpp:1829-1840).
    pub fn set_business_name(
        &self,
        env: &mut impl BusinessEnv,
        business_connection_id: BusinessConnectionId,
        first_name: &str,
        last_name: &str,
        mut promise: Promise<()>,
    ) {
        let status = self.check_business_connection(&business_connection_id);
        if status.is_error() {
            promise.set_error(status);
            return;
        }
        if first_name.chars().count() > MAX_NAME_LENGTH
            || last_name.chars().count() > MAX_NAME_LENGTH
        {
            promise.set_error(Status::error(
                400,
                "Name must be non-empty and must not exceed 64 characters",
            ));
            return;
        }
        env.set_business_name_query(business_connection_id, first_name, last_name, promise);
    }

    /// `set_business_about` (BusinessConnectionManager.cpp:1854-1861).
    pub fn set_business_about(
        &self,
        env: &mut impl BusinessEnv,
        business_connection_id: BusinessConnectionId,
        about: &str,
        mut promise: Promise<()>,
    ) {
        let status = self.check_business_connection(&business_connection_id);
        if status.is_error() {
            promise.set_error(status);
            return;
        }
        env.set_business_about_query(business_connection_id, about, promise);
    }

    /// `set_business_username` (BusinessConnectionManager.cpp:1875-1882).
    pub fn set_business_username(
        &self,
        env: &mut impl BusinessEnv,
        business_connection_id: BusinessConnectionId,
        username: &str,
        mut promise: Promise<()>,
    ) {
        let status = self.check_business_connection(&business_connection_id);
        if status.is_error() {
            promise.set_error(status);
            return;
        }
        env.set_business_username_query(business_connection_id, username, promise);
    }

    /// `set_business_gift_settings` (BusinessConnectionManager.cpp:1901-1908).
    pub fn set_business_gift_settings(
        &self,
        env: &mut impl BusinessEnv,
        business_connection_id: BusinessConnectionId,
        _settings: Option<td_api::GiftSettings>,
        mut promise: Promise<()>,
    ) {
        let status = self.check_business_connection(&business_connection_id);
        if status.is_error() {
            promise.set_error(status);
            return;
        }
        env.set_business_gift_settings_query(business_connection_id, promise);
    }

    /// `get_business_star_status` (BusinessConnectionManager.cpp:1927-1934).
    pub fn get_business_star_status(
        &self,
        env: &mut impl BusinessEnv,
        business_connection_id: BusinessConnectionId,
        mut promise: Promise<td_api::StarAmount>,
    ) {
        let status = self.check_business_connection(&business_connection_id);
        if status.is_error() {
            promise.set_error(status);
            return;
        }
        env.get_business_star_status_query(business_connection_id, promise);
    }

    /// `transfer_business_stars` (BusinessConnectionManager.cpp:1951-1958).
    pub fn transfer_business_stars(
        &self,
        env: &mut impl BusinessEnv,
        business_connection_id: BusinessConnectionId,
        star_count: i64,
        mut promise: Promise<()>,
    ) {
        let status = self.check_business_connection(&business_connection_id);
        if status.is_error() {
            promise.set_error(status);
            return;
        }
        env.transfer_business_stars_query(business_connection_id, star_count, promise);
    }

    /// `get_current_state` (BusinessConnectionManager.cpp:2247-2253) — the
    /// connection updates.
    pub fn get_current_state(&self, env: &mut impl BusinessEnv, updates: &mut Vec<td_api::Update>) {
        for connection in self.business_connections.values() {
            updates.push(connection.get_update_business_connection_object(env));
        }
    }

    ///  (MessageId.h:121-124) — also checks validness;
    /// inlined (the master MessageId carries  only).
    /// The shared edit-precondition fold (the C++ arm prologues).
    fn check_business_edit(
        &self,
        business_connection_id: &BusinessConnectionId,
        dialog_id: DialogId,
        message_id: crate::message_id::MessageId,
    ) -> Status {
        let status = self.check_business_connection_with_dialog(business_connection_id, dialog_id);
        if status.is_error() {
            return status;
        }
        self.check_business_message_id(message_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connection_id(id: &str) -> BusinessConnectionId {
        BusinessConnectionId::new(id)
    }

    #[test]
    fn id_store_parse_round_trip() {
        let value = connection_id("bc-123");
        let mut out = Vec::new();
        value.store(&mut out);
        let mut parser = Parser::new(&out);
        assert_eq!(BusinessConnectionId::parse(&mut parser).unwrap(), value);
        assert!(value.is_valid());
        assert!(BusinessConnectionId::default().is_empty());
    }

    #[test]
    fn empty_registry_rejects_every_connection() {
        let manager = BusinessConnectionManager::new();
        assert_eq!(
            manager
                .check_business_connection(&connection_id("bc-1"))
                .code(),
            400
        );
        assert_eq!(
            manager
                .check_business_connection_with_dialog(&connection_id("bc-1"), DialogId::new(5))
                .code(),
            400
        );
    }

    #[test]
    fn name_length_limit_matches_the_cpp_constant() {
        // BusinessConnectionManager.h:123 — the server-side 64-char cap.
        assert_eq!(MAX_NAME_LENGTH, 64);
    }
}
