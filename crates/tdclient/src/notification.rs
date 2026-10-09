//! Port of the notification model (M.13.1a): `NotificationId.h`,
//! `NotificationGroupId.h`, `NotificationObjectId.h`,
//! `NotificationObjectFullId.h`, `NotificationGroupKey.h`,
//! `NotificationGroupFromDatabase.h`, `NotificationGroupType.{h,cpp}`,
//! `NotificationGroupInfo.{h,cpp}`, `NotificationType.{h,cpp}` and
//! `Notification.h` from the local C++ checkout, which is newer than
//! 1.8.67 (the `chatId` wrapper type is gone — chat ids are plain `int53`;
//! `notificationTypeNewPushMessage` carries `sender_id` and an `int53`
//! `message_id`; `updateNotificationGroup` gained `notification_sound_id`).
//!
//! The `td_` surface of `NotificationType` (message objects, push media
//! objects, message senders) is the [`NotificationTypeEnv`] seam: every
//! arm answers the C++ "not loaded" `nullptr` (`None`) until its manager
//! lands — MessagesManager (M.10), FileManager/DocumentsManager (M.12)
//! and the media managers.

use std::fmt;

use crate::document::Document;
use crate::message_id::{DialogId, MessageId};
use crate::photo::{Photo, photo_get_file_ids};
use crate::store_parse::Parser;
use crate::td_api;
use crate::user_manager::{FileId, UserId};

/// `td::CallId` (CallId.h) — lives here until the Calls slice moves it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CallId(i32);

impl CallId {
    pub fn new(id: i32) -> CallId {
        CallId(id)
    }

    pub fn is_valid(self) -> bool {
        self.0 != 0
    }

    pub fn get(self) -> i32 {
        self.0
    }
}

impl fmt::Display for CallId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "call {}", self.0)
    }
}

/// `td::NotificationId` (NotificationId.h).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NotificationId(i32);

impl NotificationId {
    pub fn new(id: i32) -> NotificationId {
        NotificationId(id)
    }

    pub fn max() -> NotificationId {
        NotificationId(i32::MAX)
    }

    pub fn is_valid(self) -> bool {
        self.0 > 0
    }

    pub fn get(self) -> i32 {
        self.0
    }

    pub fn store(self, out: &mut Vec<u8>) {
        crate::store_parse::store_i32(out, self.0);
    }

    pub fn parse_value(input: &[u8]) -> Result<NotificationId, &'static str> {
        let mut parser = Parser::new(input);
        let id = parser.i32_()?;
        parser.fetch_end()?;
        Ok(NotificationId(id))
    }
}

impl fmt::Display for NotificationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "notification {}", self.0)
    }
}

/// `td::NotificationGroupId` (NotificationGroupId.h).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NotificationGroupId(i32);

impl NotificationGroupId {
    pub fn new(id: i32) -> NotificationGroupId {
        NotificationGroupId(id)
    }

    pub fn is_valid(self) -> bool {
        self.0 > 0
    }

    pub fn get(self) -> i32 {
        self.0
    }

    pub fn store(self, out: &mut Vec<u8>) {
        crate::store_parse::store_i32(out, self.0);
    }

    pub fn parse_value(input: &[u8]) -> Result<NotificationGroupId, &'static str> {
        let mut parser = Parser::new(input);
        let id = parser.i32_()?;
        parser.fetch_end()?;
        Ok(NotificationGroupId(id))
    }
}

impl fmt::Display for NotificationGroupId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "notification group {}", self.0)
    }
}

/// `td::NotificationObjectId` (NotificationObjectId.h) — a `MessageId`-shaped
/// i64 key used to mark removed notification objects.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NotificationObjectId(i64);

impl NotificationObjectId {
    pub fn new(id: i64) -> NotificationObjectId {
        NotificationObjectId(id)
    }

    /// `NotificationObjectId(MessageId)` (NotificationObjectId.h:24-26).
    pub fn from_message_id(message_id: MessageId) -> NotificationObjectId {
        NotificationObjectId(message_id.get())
    }

    pub fn max() -> NotificationObjectId {
        // `NotificationObjectId(MessageId::max())` (NotificationObjectId.h).
        NotificationObjectId(MessageId::max().get())
    }

    pub fn is_valid(self) -> bool {
        self.0 > 0
    }

    pub fn get(self) -> i64 {
        self.0
    }

    pub fn store(self, out: &mut Vec<u8>) {
        crate::store_parse::store_i64(out, self.0);
    }

    pub fn parse_value(input: &[u8]) -> Result<NotificationObjectId, &'static str> {
        let mut parser = Parser::new(input);
        let id = parser.i64_()?;
        parser.fetch_end()?;
        Ok(NotificationObjectId(id))
    }
}

impl fmt::Display for NotificationObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "notification object {}", self.0)
    }
}

/// `td::NotificationObjectFullId` (NotificationObjectFullId.h).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct NotificationObjectFullId {
    dialog_id: DialogId,
    notification_object_id: NotificationObjectId,
}

impl NotificationObjectFullId {
    pub fn new(
        dialog_id: DialogId,
        notification_object_id: NotificationObjectId,
    ) -> NotificationObjectFullId {
        NotificationObjectFullId {
            dialog_id,
            notification_object_id,
        }
    }

    pub fn get_dialog_id(self) -> DialogId {
        self.dialog_id
    }

    pub fn get_notification_object_id(self) -> NotificationObjectId {
        self.notification_object_id
    }
}

impl fmt::Display for NotificationObjectFullId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} in dialog {}",
            self.notification_object_id,
            self.dialog_id.get()
        )
    }
}

/// `td::NotificationGroupKey` (NotificationGroupKey.h) — the sort key for
/// the group list; the C++ `operator<` orders by *descending* notification
/// date, then descending dialog id, then descending group id (most recent
/// first).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NotificationGroupKey {
    pub group_id: NotificationGroupId,
    pub dialog_id: DialogId,
    pub last_notification_date: i32,
}

impl NotificationGroupKey {
    pub fn new(
        group_id: NotificationGroupId,
        dialog_id: DialogId,
        last_notification_date: i32,
    ) -> NotificationGroupKey {
        NotificationGroupKey {
            group_id,
            dialog_id,
            last_notification_date,
        }
    }
}

impl PartialOrd for NotificationGroupKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for NotificationGroupKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        if self.last_notification_date != other.last_notification_date {
            return other
                .last_notification_date
                .cmp(&self.last_notification_date);
        }
        if self.dialog_id != other.dialog_id {
            return other.dialog_id.get().cmp(&self.dialog_id.get());
        }
        other.group_id.get().cmp(&self.group_id.get())
    }
}

impl fmt::Display for NotificationGroupKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "[{},dialog {},{}]",
            self.group_id,
            self.dialog_id.get(),
            self.last_notification_date
        )
    }
}

/// `td::NotificationGroupType` (NotificationGroupType.h/.cpp).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum NotificationGroupType {
    #[default]
    Messages,
    Mentions,
    SecretChat,
    Calls,
}

impl NotificationGroupType {
    /// `is_database_notification_group_type` (NotificationGroupType.cpp).
    pub fn is_database_notification_group_type(self) -> bool {
        match self {
            Self::Messages | Self::Mentions | Self::SecretChat => true,
            Self::Calls => false,
        }
    }

    /// `is_partial_notification_group_type` (NotificationGroupType.cpp).
    pub fn is_partial_notification_group_type(self) -> bool {
        match self {
            Self::Messages | Self::Mentions => true,
            Self::SecretChat | Self::Calls => false,
        }
    }

    /// `get_notification_group_type_object` (NotificationGroupType.cpp).
    pub fn get_notification_group_type_object(self) -> td_api::NotificationGroupType {
        match self {
            Self::Messages => td_api::NotificationGroupType::notificationGroupTypeMessages(
                td_api::notificationGroupTypeMessages,
            ),
            Self::Mentions => td_api::NotificationGroupType::notificationGroupTypeMentions(
                td_api::notificationGroupTypeMentions,
            ),
            Self::SecretChat => td_api::NotificationGroupType::notificationGroupTypeSecretChat(
                td_api::notificationGroupTypeSecretChat,
            ),
            Self::Calls => td_api::NotificationGroupType::notificationGroupTypeCalls(
                td_api::notificationGroupTypeCalls,
            ),
        }
    }

    /// `get_notification_group_type` (NotificationGroupType.cpp).
    pub fn get_notification_group_type(
        type_: &td_api::NotificationGroupType,
    ) -> NotificationGroupType {
        match type_ {
            td_api::NotificationGroupType::notificationGroupTypeMessages(_) => Self::Messages,
            td_api::NotificationGroupType::notificationGroupTypeMentions(_) => Self::Mentions,
            td_api::NotificationGroupType::notificationGroupTypeSecretChat(_) => Self::SecretChat,
            td_api::NotificationGroupType::notificationGroupTypeCalls(_) => Self::Calls,
        }
    }
}

impl fmt::Display for NotificationGroupType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Messages => write!(f, "Messages"),
            Self::Mentions => write!(f, "Mentions"),
            Self::SecretChat => write!(f, "SecretChat"),
            Self::Calls => write!(f, "Calls"),
        }
    }
}

/// `td::NotificationGroupInfo` (NotificationGroupInfo.{h,hpp,cpp}) — the
/// per-dialog notification group cursor, including the removed-notification
/// watermark logic that filters late server pushes.
#[derive(Clone, Copy, Debug, Default)]
pub struct NotificationGroupInfo {
    group_id: NotificationGroupId,
    last_notification_date: i32,
    last_notification_id: NotificationId,
    max_removed_notification_id: NotificationId,
    max_removed_object_id: NotificationObjectId,
    is_key_changed: bool,
    try_reuse: bool,
}

impl NotificationGroupInfo {
    /// `NotificationGroupInfo(NotificationGroupId)` (NotificationGroupInfo.h:34-35).
    pub fn with_group_id(group_id: NotificationGroupId) -> NotificationGroupInfo {
        NotificationGroupInfo {
            group_id,
            is_key_changed: true,
            ..NotificationGroupInfo::default()
        }
    }

    pub fn is_valid(self) -> bool {
        self.group_id.is_valid()
    }

    pub fn is_active(self) -> bool {
        self.is_valid() && !self.try_reuse
    }

    pub fn get_group_id(self) -> NotificationGroupId {
        self.group_id
    }

    pub fn has_group_id(self, group_id: NotificationGroupId) -> bool {
        self.group_id == group_id
    }

    pub fn get_last_notification_id(self) -> NotificationId {
        self.last_notification_id
    }

    /// `NotificationGroupInfo::set_last_notification` (NotificationGroupInfo.cpp:15-32).
    pub fn set_last_notification(
        &mut self,
        mut last_notification_date: i32,
        mut last_notification_id: NotificationId,
    ) -> bool {
        if self.is_removed_notification_id(last_notification_id) {
            last_notification_id = NotificationId::default();
            last_notification_date = 0;
        }
        if self.last_notification_date != last_notification_date
            || self.last_notification_id != last_notification_id
        {
            if self.last_notification_date != last_notification_date {
                self.last_notification_date = last_notification_date;
                self.is_key_changed = true;
            }
            self.last_notification_id = last_notification_id;
            return true;
        }
        false
    }

    /// `NotificationGroupInfo::set_max_removed_notification_id`
    /// (NotificationGroupInfo.cpp:34-57).
    pub fn set_max_removed_notification_id(
        &mut self,
        max_removed_notification_id: NotificationId,
        max_removed_object_id: NotificationObjectId,
    ) -> bool {
        if max_removed_notification_id.get() <= self.max_removed_notification_id.get() {
            return false;
        }
        if max_removed_object_id > self.max_removed_object_id {
            self.max_removed_object_id = max_removed_object_id;
        }

        self.max_removed_notification_id = max_removed_notification_id;

        if self.last_notification_id.is_valid()
            && self.is_removed_notification_id(self.last_notification_id)
        {
            self.last_notification_id = NotificationId::default();
            self.last_notification_date = 0;
            self.is_key_changed = true;
        }

        true
    }

    /// `NotificationGroupInfo::drop_max_removed_notification_id`
    /// (NotificationGroupInfo.cpp:59-67).
    pub fn drop_max_removed_notification_id(&mut self) {
        if !self.max_removed_notification_id.is_valid() {
            return;
        }

        self.max_removed_object_id = NotificationObjectId::default();
        self.max_removed_notification_id = NotificationId::default();
    }

    /// `NotificationGroupInfo::is_removed_notification` (NotificationGroupInfo.cpp:69-72).
    pub fn is_removed_notification(
        self,
        notification_id: NotificationId,
        object_id: NotificationObjectId,
    ) -> bool {
        self.is_removed_notification_id(notification_id) || self.is_removed_object_id(object_id)
    }

    /// `NotificationGroupInfo::is_removed_notification_id` (NotificationGroupInfo.cpp:74-76).
    pub fn is_removed_notification_id(self, notification_id: NotificationId) -> bool {
        notification_id.get() <= self.max_removed_notification_id.get()
    }

    /// `NotificationGroupInfo::is_removed_object_id` (NotificationGroupInfo.cpp:78-80).
    pub fn is_removed_object_id(self, object_id: NotificationObjectId) -> bool {
        object_id <= self.max_removed_object_id
    }

    /// `NotificationGroupInfo::is_used_notification_id` (NotificationGroupInfo.cpp:82-85).
    pub fn is_used_notification_id(self, notification_id: NotificationId) -> bool {
        notification_id.get() <= self.max_removed_notification_id.get()
            || notification_id.get() <= self.last_notification_id.get()
    }

    /// `NotificationGroupInfo::try_reuse` (NotificationGroupInfo.cpp:87-94).
    pub fn try_reuse(&mut self) {
        assert!(self.is_valid());
        assert!(self.last_notification_date == 0);
        if !self.try_reuse {
            self.try_reuse = true;
            self.is_key_changed = true;
        }
    }

    /// `NotificationGroupInfo::add_group_key_if_changed` (NotificationGroupInfo.cpp:96-103).
    pub fn add_group_key_if_changed(
        &mut self,
        group_keys: &mut Vec<NotificationGroupKey>,
        dialog_id: DialogId,
    ) {
        if !self.is_key_changed {
            return;
        }
        self.is_key_changed = false;

        group_keys.push(NotificationGroupKey::new(
            self.group_id,
            if self.try_reuse {
                DialogId::default()
            } else {
                dialog_id
            },
            self.last_notification_date,
        ));
    }

    /// `NotificationGroupInfo::get_reused_group_id` (NotificationGroupInfo.cpp:105-125).
    pub fn get_reused_group_id(&mut self) -> NotificationGroupId {
        if !self.try_reuse {
            return NotificationGroupId::default();
        }
        if self.is_key_changed {
            tdutils::log_error!("Failed to reuse changed {}", self.group_id);
            return NotificationGroupId::default();
        }
        self.try_reuse = false;
        if !self.is_valid() {
            tdutils::log_error!("Failed to reuse invalid {}", self.group_id);
            return NotificationGroupId::default();
        }
        assert!(self.last_notification_id == NotificationId::default());
        assert!(self.last_notification_date == 0);
        let result = self.group_id;
        self.group_id = NotificationGroupId::default();
        self.max_removed_notification_id = NotificationId::default();
        self.max_removed_object_id = NotificationObjectId::default();
        result
    }

    /// `NotificationGroupInfo::store` (NotificationGroupInfo.hpp:17-25).
    pub fn store(self, out: &mut Vec<u8>) {
        self.group_id.store(out);
        crate::store_parse::store_i32(out, self.last_notification_date);
        self.last_notification_id.store(out);
        self.max_removed_notification_id.store(out);
        self.max_removed_object_id.store(out);
    }

    /// `NotificationGroupInfo::parse` (NotificationGroupInfo.hpp:27-37);
    /// `version` is the database serializer version, the object id field is
    /// read only from `AddNotificationGroupInfoMaxRemovedMessageId` (18).
    pub fn parse_value(input: &[u8], version: i32) -> Result<NotificationGroupInfo, &'static str> {
        let mut parser = Parser::new(input);
        let group_id = parser.i32_()?;
        let last_notification_date = parser.i32_()?;
        let last_notification_id = parser.i32_()?;
        let max_removed_notification_id = parser.i32_()?;
        let max_removed_object_id = if version >= ADD_NOTIFICATION_GROUP_INFO_MAX_REMOVED_MESSAGE_ID
        {
            parser.i64_()?
        } else {
            0
        };
        parser.fetch_end()?;
        Ok(NotificationGroupInfo {
            group_id: NotificationGroupId(group_id),
            last_notification_date,
            last_notification_id: NotificationId(last_notification_id),
            max_removed_notification_id: NotificationId(max_removed_notification_id),
            max_removed_object_id: NotificationObjectId(max_removed_object_id),
            is_key_changed: false,
            try_reuse: false,
        })
    }
}

/// `Version::AddNotificationGroupInfoMaxRemovedMessageId` (Version.h:35) —
/// 18th entry of the database serializer version enum.
pub const ADD_NOTIFICATION_GROUP_INFO_MAX_REMOVED_MESSAGE_ID: i32 = 18;

/// `td::NotificationType` (NotificationType.h/.cpp) — the four concrete
/// notification payloads behind the C++ polymorphic class.
#[derive(Clone, Debug, PartialEq)]
// the C++ heap-allocates every subclass; boxing the tail is the recorded
// alternative if the payload size spread ever matters
#[allow(clippy::large_enum_variant)]
pub enum NotificationType {
    /// `NotificationTypeMessage` (NotificationType.cpp:29-66).
    NewMessage {
        message_id: MessageId,
        show_preview: bool,
    },
    /// `NotificationTypeSecretChat` (NotificationType.cpp:68-96).
    NewSecretChat,
    /// `NotificationTypeCall` (NotificationType.cpp:98-128).
    NewCall { call_id: CallId },
    /// `NotificationTypePushMessage` (NotificationType.cpp:130-487).
    NewPushMessage {
        sender_user_id: UserId,
        sender_dialog_id: DialogId,
        sender_name: String,
        is_outgoing: bool,
        message_id: MessageId,
        key: String,
        arg: String,
        photo: Photo,
        document: Document,
    },
}

impl NotificationType {
    /// `create_new_message_notification` (NotificationType.cpp:489-491).
    pub fn new_message(message_id: MessageId, show_preview: bool) -> NotificationType {
        NotificationType::NewMessage {
            message_id,
            show_preview,
        }
    }

    /// `create_new_secret_chat_notification` (NotificationType.cpp:493-495).
    pub fn new_secret_chat() -> NotificationType {
        NotificationType::NewSecretChat
    }

    /// `create_new_call_notification` (NotificationType.cpp:497-499).
    pub fn new_call(call_id: CallId) -> NotificationType {
        NotificationType::NewCall { call_id }
    }

    /// `create_new_push_message_notification` (NotificationType.cpp:501-508).
    #[allow(clippy::too_many_arguments)]
    pub fn new_push_message(
        sender_user_id: UserId,
        sender_dialog_id: DialogId,
        sender_name: String,
        is_outgoing: bool,
        message_id: MessageId,
        key: String,
        arg: String,
        photo: Photo,
        document: Document,
    ) -> NotificationType {
        NotificationType::NewPushMessage {
            sender_user_id,
            sender_dialog_id,
            sender_name,
            is_outgoing,
            message_id,
            key,
            arg,
            photo,
            document,
        }
    }

    /// `NotificationType::can_be_delayed` — only server messages can be.
    pub fn can_be_delayed(&self) -> bool {
        match self {
            Self::NewMessage { message_id, .. } => message_id_is_server(*message_id),
            Self::NewSecretChat | Self::NewCall { .. } | Self::NewPushMessage { .. } => false,
        }
    }

    /// `NotificationType::is_temporary` — only push messages are.
    pub fn is_temporary(&self) -> bool {
        matches!(self, Self::NewPushMessage { .. })
    }

    /// `NotificationType::get_object_id`.
    pub fn get_object_id(&self) -> NotificationObjectId {
        match self {
            Self::NewMessage { message_id, .. } => {
                NotificationObjectId::from_message_id(*message_id)
            }
            Self::NewSecretChat => NotificationObjectId::default(),
            Self::NewCall { .. } => NotificationObjectId::max(),
            Self::NewPushMessage { message_id, .. } => {
                NotificationObjectId::from_message_id(*message_id)
            }
        }
    }

    /// `NotificationType::get_file_ids` — only push messages carry files;
    /// the `Document` shim returns its single file id until the full
    /// `Document::get_file_ids` lands with M.9.
    pub fn get_file_ids(&self) -> Vec<FileId> {
        match self {
            Self::NewPushMessage {
                document, photo, ..
            } => {
                if !document.is_empty() {
                    return vec![document.file_id];
                }
                photo_get_file_ids(photo)
            }
            _ => Vec::new(),
        }
    }

    /// `NotificationType::get_notification_type_object` — `None` mirrors the
    /// C++ `nullptr` (the message / sender is not loaded), and the callers
    /// drop the whole notification then.
    pub fn get_notification_type_object(
        &self,
        env: &impl NotificationTypeEnv,
        dialog_id: DialogId,
    ) -> Option<td_api::NotificationType> {
        match self {
            Self::NewMessage {
                message_id,
                show_preview,
            } => {
                let message = env.get_message_object(dialog_id, *message_id)?;
                Some(td_api::NotificationType::notificationTypeNewMessage(
                    td_api::notificationTypeNewMessage {
                        message: Some(message),
                        show_preview: *show_preview,
                    },
                ))
            }
            Self::NewSecretChat => Some(td_api::NotificationType::notificationTypeNewSecretChat(
                td_api::notificationTypeNewSecretChat,
            )),
            Self::NewCall { call_id } => Some(td_api::NotificationType::notificationTypeNewCall(
                td_api::notificationTypeNewCall {
                    call_id: call_id.get(),
                },
            )),
            Self::NewPushMessage {
                sender_user_id,
                sender_dialog_id,
                sender_name,
                is_outgoing,
                message_id,
                key,
                arg,
                photo,
                document,
            } => Some(td_api::NotificationType::notificationTypeNewPushMessage(
                td_api::notificationTypeNewPushMessage {
                    message_id: message_id.get(),
                    sender_id: env.get_message_sender_object(*sender_user_id, *sender_dialog_id),
                    sender_name: sender_name.clone(),
                    is_outgoing: *is_outgoing,
                    content: Some(get_push_message_content_object(
                        env,
                        key,
                        arg,
                        *sender_user_id,
                        photo,
                        document,
                    )),
                },
            )),
        }
    }
}

impl fmt::Display for NotificationType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NewMessage { message_id, .. } => {
                write!(f, "NewMessageNotification[message {}]", message_id.get())
            }
            Self::NewSecretChat => write!(f, "NewSecretChatNotification[]"),
            Self::NewCall { call_id } => write!(f, "NewCallNotification[{}]", call_id),
            Self::NewPushMessage {
                sender_user_id,
                sender_dialog_id,
                sender_name,
                message_id,
                key,
                arg,
                photo,
                document,
                ..
            } => write!(
                f,
                "NewPushMessageNotification[{}/dialog {}/\"{}\", message {}, {}, {}, {:?}, {:?}]",
                sender_user_id.get(),
                sender_dialog_id.get(),
                sender_name,
                message_id.get(),
                key,
                arg,
                photo,
                document
            ),
        }
    }
}

/// `MessageId::is_server` (MessageId.h) — the full layout port arrives with
/// M.10's MessageId; this is the subset `can_be_delayed` reads: valid,
/// not scheduled, and with clear type bits.
fn message_id_is_server(message_id: MessageId) -> bool {
    message_id.get() > 0 && !message_id.is_scheduled() && (message_id.get() & 3) == 0
}

/// `td::Notification` (Notification.h) — the C++ `type` member is a
/// `unique_ptr` that is never null in practice (the store/parse CHECKs it).
#[derive(Clone, Debug, PartialEq)]
pub struct Notification {
    pub notification_id: NotificationId,
    pub date: i32,
    pub disable_notification: bool,
    pub r#type: NotificationType,
}

impl Notification {
    pub fn new(
        notification_id: NotificationId,
        date: i32,
        disable_notification: bool,
        type_: NotificationType,
    ) -> Notification {
        Notification {
            notification_id,
            date,
            disable_notification,
            r#type: type_,
        }
    }
}

impl fmt::Display for Notification {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "notification[{}, {}, {}, {}]",
            self.notification_id, self.date, self.disable_notification, self.r#type
        )
    }
}

/// `td::NotificationGroupFromDatabase` (NotificationGroupFromDatabase.h);
/// the C++ default group type is `Calls` (h:13).
#[derive(Clone, Debug)]
pub struct NotificationGroupFromDatabase {
    pub dialog_id: DialogId,
    pub r#type: NotificationGroupType,
    pub total_count: i32,
    pub notifications: Vec<Notification>,
}

impl Default for NotificationGroupFromDatabase {
    fn default() -> NotificationGroupFromDatabase {
        NotificationGroupFromDatabase {
            dialog_id: DialogId::default(),
            r#type: NotificationGroupType::Calls,
            total_count: 0,
            notifications: Vec::new(),
        }
    }
}

/// `get_notification_object` (Notification.h:47-53) — builds the td_api
/// object; a not-loaded message makes `r#type` `None`, exactly like the C++
/// null `type_`, and the callers drop the notification.
pub fn get_notification_object(
    env: &impl NotificationTypeEnv,
    dialog_id: DialogId,
    notification: &Notification,
) -> td_api::Notification {
    td_api::Notification {
        id: notification.notification_id.get(),
        date: notification.date,
        is_silent: notification.disable_notification,
        r#type: notification
            .r#type
            .get_notification_type_object(env, dialog_id),
    }
}

/// The `td_`/media-manager surface `NotificationType.cpp` reads. Every arm
/// is a seam answering the C++ "not loaded" result (`nullptr` → `None`)
/// until its manager lands.
pub trait NotificationTypeEnv {
    /// `td_->messages_manager_->get_message_object` — seam until M.10.
    fn get_message_object(
        &self,
        _dialog_id: DialogId,
        _message_id: MessageId,
    ) -> Option<td_api::Message> {
        None
    }

    /// `get_message_sender_object` (MessageSender.cpp) — seam until the
    /// MessagesManager/DialogManager intakes land.
    fn get_message_sender_object(
        &self,
        _sender_user_id: UserId,
        _sender_dialog_id: DialogId,
    ) -> Option<td_api::MessageSender> {
        None
    }

    /// `td_->animations_manager_->get_animation_object` — seam.
    fn get_animation_object(&self, _file_id: FileId) -> Option<td_api::Animation> {
        None
    }

    /// `td_->audios_manager_->get_audio_object` — seam.
    fn get_audio_object(&self, _file_id: FileId) -> Option<td_api::Audio> {
        None
    }

    /// `td_->documents_manager_->get_document_object` — seam.
    fn get_document_object(&self, _file_id: FileId) -> Option<td_api::Document> {
        None
    }

    /// `td_->stickers_manager_->get_sticker_object` — seam.
    fn get_sticker_object(&self, _file_id: FileId) -> Option<td_api::Sticker> {
        None
    }

    /// `td_->video_notes_manager_->get_video_note_object` — seam.
    fn get_video_note_object(&self, _file_id: FileId) -> Option<td_api::VideoNote> {
        None
    }

    /// `td_->videos_manager_->get_video_object` — seam.
    fn get_video_object(&self, _file_id: FileId) -> Option<td_api::Video> {
        None
    }

    /// `td_->voice_notes_manager_->get_voice_note_object` — seam.
    fn get_voice_note_object(&self, _file_id: FileId) -> Option<td_api::VoiceNote> {
        None
    }

    /// `get_photo_object` (FileManager) — seam until the Files step (M.12).
    fn get_photo_object(&self, _photo: &Photo) -> Option<td_api::Photo> {
        None
    }
}

/// `td::split` (misc.h) — split at the first space; the tail keeps the rest.
fn split_first_space(s: &str) -> (&str, &str) {
    match s.find(' ') {
        Some(pos) => (&s[..pos], &s[pos + 1..]),
        None => (s, ""),
    }
}

/// `td::to_integer<int32>` (misc.h) — optional leading '-', leading digits
/// only, two's-complement wrap on overflow.
fn to_integer_i32(s: &str) -> i32 {
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut is_negative = false;
    if i < bytes.len() && bytes[i] == b'-' {
        is_negative = true;
        i += 1;
    }
    let mut value: u32 = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        value = value
            .wrapping_mul(10)
            .wrapping_add((bytes[i] - b'0') as u32);
        i += 1;
    }
    if value > i32::MAX as u32 {
        value = value.wrapping_neg();
        is_negative = !is_negative;
        if value > i32::MAX as u32 {
            return i32::MIN;
        }
    }
    let value = value as i32;
    if is_negative { -value } else { value }
}

/// `td::to_integer<int64>` (misc.h) — see [`to_integer_i32`].
fn to_integer_i64(s: &str) -> i64 {
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut is_negative = false;
    if i < bytes.len() && bytes[i] == b'-' {
        is_negative = true;
        i += 1;
    }
    let mut value: u64 = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        value = value
            .wrapping_mul(10)
            .wrapping_add((bytes[i] - b'0') as u64);
        i += 1;
    }
    if value > i64::MAX as u64 {
        value = value.wrapping_neg();
        is_negative = !is_negative;
        if value > i64::MAX as u64 {
            return i64::MIN;
        }
    }
    let value = value as i64;
    if is_negative { -value } else { value }
}

/// `td::trim` (Slice.h) — trims ASCII whitespace (`<= 0x20`) from both ends.
fn trim_ascii(s: &str) -> &str {
    s.trim_matches(|c: char| (c as u32) <= 0x20)
}

/// `NotificationTypePushMessage::get_push_message_content_object`
/// (NotificationType.cpp:151-449) — the loc-key → td_api push content
/// mapping. Manager-backed sub-objects come from the env seam.
#[allow(clippy::too_many_lines)]
fn get_push_message_content_object(
    env: &impl NotificationTypeEnv,
    key: &str,
    arg: &str,
    sender_user_id: UserId,
    photo: &Photo,
    document: &Document,
) -> td_api::PushMessageContent {
    let mut is_pinned = false;
    let key = match key.strip_prefix("PINNED_") {
        Some(stripped) => {
            is_pinned = true;
            stripped
        }
        None => key,
    };
    if key == "MESSAGE" {
        return td_api::PushMessageContent::pushMessageContentHidden(
            td_api::pushMessageContentHidden { is_pinned },
        );
    }
    if key == "MESSAGES" {
        return td_api::PushMessageContent::pushMessageContentMediaAlbum(
            td_api::pushMessageContentMediaAlbum {
                total_count: to_integer_i32(arg),
                has_photos: true,
                has_videos: true,
                has_audios: false,
                has_documents: false,
            },
        );
    }
    assert!(key.len() > 8, "short push notification key {key}");
    match key.as_bytes()[8] {
        b'A' => {
            if key == "MESSAGE_ANIMATION" {
                return td_api::PushMessageContent::pushMessageContentAnimation(
                    td_api::pushMessageContentAnimation {
                        animation: env.get_animation_object(document.file_id),
                        caption: arg.to_string(),
                        is_pinned,
                    },
                );
            }
            if key == "MESSAGE_AUDIO" {
                return td_api::PushMessageContent::pushMessageContentAudio(
                    td_api::pushMessageContentAudio {
                        audio: env.get_audio_object(document.file_id),
                        is_pinned,
                    },
                );
            }
            if key == "MESSAGE_AUDIOS" {
                return td_api::PushMessageContent::pushMessageContentMediaAlbum(
                    td_api::pushMessageContentMediaAlbum {
                        total_count: to_integer_i32(arg),
                        has_photos: false,
                        has_videos: false,
                        has_audios: true,
                        has_documents: false,
                    },
                );
            }
        }
        b'B' if key == "MESSAGE_BASIC_GROUP_CHAT_CREATE" => {
            return td_api::PushMessageContent::pushMessageContentBasicGroupChatCreate(
                td_api::pushMessageContentBasicGroupChatCreate,
            );
        }
        b'C' => {
            if key == "MESSAGE_CHAT_ADD_MEMBERS" {
                return td_api::PushMessageContent::pushMessageContentChatAddMembers(
                    td_api::pushMessageContentChatAddMembers {
                        member_name: arg.to_string(),
                        is_current_user: false,
                        is_returned: false,
                    },
                );
            }
            if key == "MESSAGE_CHAT_ADD_MEMBERS_RETURNED" {
                return td_api::PushMessageContent::pushMessageContentChatAddMembers(
                    td_api::pushMessageContentChatAddMembers {
                        member_name: arg.to_string(),
                        is_current_user: false,
                        is_returned: true,
                    },
                );
            }
            if key == "MESSAGE_CHAT_ADD_MEMBERS_YOU" {
                return td_api::PushMessageContent::pushMessageContentChatAddMembers(
                    td_api::pushMessageContentChatAddMembers {
                        member_name: arg.to_string(),
                        is_current_user: true,
                        is_returned: false,
                    },
                );
            }
            if key == "MESSAGE_CHAT_CHANGE_PHOTO" {
                return td_api::PushMessageContent::pushMessageContentChatChangePhoto(
                    td_api::pushMessageContentChatChangePhoto,
                );
            }
            if key == "MESSAGE_CHAT_CHANGE_THEME" {
                return td_api::PushMessageContent::pushMessageContentChatSetTheme(
                    td_api::pushMessageContentChatSetTheme {
                        name: arg.to_string(),
                    },
                );
            }
            if key == "MESSAGE_CHAT_CHANGE_TITLE" {
                return td_api::PushMessageContent::pushMessageContentChatChangeTitle(
                    td_api::pushMessageContentChatChangeTitle {
                        title: arg.to_string(),
                    },
                );
            }
            if key == "MESSAGE_CHAT_DELETE_MEMBER" {
                return td_api::PushMessageContent::pushMessageContentChatDeleteMember(
                    td_api::pushMessageContentChatDeleteMember {
                        member_name: arg.to_string(),
                        is_current_user: false,
                        is_left: false,
                    },
                );
            }
            if key == "MESSAGE_CHAT_DELETE_MEMBER_LEFT" {
                return td_api::PushMessageContent::pushMessageContentChatDeleteMember(
                    td_api::pushMessageContentChatDeleteMember {
                        member_name: arg.to_string(),
                        is_current_user: false,
                        is_left: true,
                    },
                );
            }
            if key == "MESSAGE_CHAT_DELETE_MEMBER_YOU" {
                return td_api::PushMessageContent::pushMessageContentChatDeleteMember(
                    td_api::pushMessageContentChatDeleteMember {
                        member_name: arg.to_string(),
                        is_current_user: true,
                        is_left: false,
                    },
                );
            }
            if key == "MESSAGE_CHAT_JOIN_BY_LINK" {
                return td_api::PushMessageContent::pushMessageContentChatJoinByLink(
                    td_api::pushMessageContentChatJoinByLink,
                );
            }
            if key == "MESSAGE_CHAT_JOIN_BY_REQUEST" {
                return td_api::PushMessageContent::pushMessageContentChatJoinByRequest(
                    td_api::pushMessageContentChatJoinByRequest,
                );
            }
            if key == "MESSAGE_CHAT_LIVESTREAM_END" {
                return td_api::PushMessageContent::pushMessageContentVideoChatEnded(
                    td_api::pushMessageContentVideoChatEnded,
                );
            }
            if key == "MESSAGE_CHAT_LIVESTREAM_START" {
                return td_api::PushMessageContent::pushMessageContentVideoChatStarted(
                    td_api::pushMessageContentVideoChatStarted,
                );
            }
            if key == "MESSAGE_CHAT_VOICECHAT_END" {
                return td_api::PushMessageContent::pushMessageContentVideoChatEnded(
                    td_api::pushMessageContentVideoChatEnded,
                );
            }
            if key == "MESSAGE_CHAT_VOICECHAT_INVITE" {
                return td_api::PushMessageContent::pushMessageContentInviteVideoChatParticipants(
                    td_api::pushMessageContentInviteVideoChatParticipants {
                        is_current_user: false,
                    },
                );
            }
            if key == "MESSAGE_CHAT_VOICECHAT_INVITE_YOU" {
                return td_api::PushMessageContent::pushMessageContentInviteVideoChatParticipants(
                    td_api::pushMessageContentInviteVideoChatParticipants {
                        is_current_user: true,
                    },
                );
            }
            if key == "MESSAGE_CHAT_VOICECHAT_START" {
                return td_api::PushMessageContent::pushMessageContentVideoChatStarted(
                    td_api::pushMessageContentVideoChatStarted,
                );
            }
            if key == "MESSAGE_CONTACT" {
                return td_api::PushMessageContent::pushMessageContentContact(
                    td_api::pushMessageContentContact {
                        name: arg.to_string(),
                        is_pinned,
                    },
                );
            }
            if key == "MESSAGE_CONTACT_REGISTERED" {
                return td_api::PushMessageContent::pushMessageContentContactRegistered(
                    td_api::pushMessageContentContactRegistered {
                        as_premium_account: false,
                    },
                );
            }
            if key == "MESSAGE_CONTACT_REGISTERED_PREMIUM" {
                return td_api::PushMessageContent::pushMessageContentContactRegistered(
                    td_api::pushMessageContentContactRegistered {
                        as_premium_account: true,
                    },
                );
            }
        }
        b'D' => {
            if key == "MESSAGE_DOCUMENT" {
                return td_api::PushMessageContent::pushMessageContentDocument(
                    td_api::pushMessageContentDocument {
                        document: env.get_document_object(document.file_id),
                        is_pinned,
                    },
                );
            }
            if key == "MESSAGE_DOCUMENTS" {
                return td_api::PushMessageContent::pushMessageContentMediaAlbum(
                    td_api::pushMessageContentMediaAlbum {
                        total_count: to_integer_i32(arg),
                        has_photos: false,
                        has_videos: false,
                        has_audios: false,
                        has_documents: true,
                    },
                );
            }
        }
        b'F' if key == "MESSAGE_FORWARDS" => {
            return td_api::PushMessageContent::pushMessageContentMessageForwards(
                td_api::pushMessageContentMessageForwards {
                    total_count: to_integer_i32(arg),
                },
            );
        }
        b'G' => {
            if key == "MESSAGE_GAME" {
                return td_api::PushMessageContent::pushMessageContentGame(
                    td_api::pushMessageContentGame {
                        title: arg.to_string(),
                        is_pinned,
                    },
                );
            }
            if key == "MESSAGE_GAME_SCORE" {
                let mut score = 0;
                let mut title = String::new();
                if !is_pinned {
                    let (score_str, title_str) = split_first_space(arg);
                    score = to_integer_i32(score_str);
                    title = title_str.to_string();
                }
                return td_api::PushMessageContent::pushMessageContentGameScore(
                    td_api::pushMessageContentGameScore {
                        title,
                        score,
                        is_pinned,
                    },
                );
            }
            if key == "MESSAGE_GIFTCODE" {
                return td_api::PushMessageContent::pushMessageContentPremiumGiftCode(
                    td_api::pushMessageContentPremiumGiftCode {
                        month_count: to_integer_i32(arg),
                    },
                );
            }
            if key == "MESSAGE_GIVEAWAY" {
                let mut user_count = 0;
                let mut month_count = 0;
                if !is_pinned {
                    let (user_count_str, month_count_str) = split_first_space(arg);
                    user_count = to_integer_i32(user_count_str);
                    month_count = to_integer_i32(month_count_str);
                }
                return td_api::PushMessageContent::pushMessageContentGiveaway(
                    td_api::pushMessageContentGiveaway {
                        winner_count: user_count,
                        prize: if is_pinned {
                            None
                        } else {
                            Some(td_api::GiveawayPrize::giveawayPrizePremium(
                                td_api::giveawayPrizePremium { month_count },
                            ))
                        },
                        is_pinned,
                    },
                );
            }
            if key == "MESSAGE_GIVEAWAY_STARS" {
                let mut user_count = 0;
                let mut star_count = 0;
                if !is_pinned {
                    let (user_count_str, star_count_str) = split_first_space(arg);
                    user_count = to_integer_i32(user_count_str);
                    star_count = to_integer_i64(star_count_str);
                }
                return td_api::PushMessageContent::pushMessageContentGiveaway(
                    td_api::pushMessageContentGiveaway {
                        winner_count: user_count,
                        prize: if is_pinned {
                            None
                        } else {
                            Some(td_api::GiveawayPrize::giveawayPrizeStars(
                                td_api::giveawayPrizeStars { star_count },
                            ))
                        },
                        is_pinned,
                    },
                );
            }
            if key == "MESSAGE_GRAM_TRANSFER" {
                // NotificationType.cpp:309-318 — the amount and the
                // (possibly empty) comment ride one \xFF-separated arg.
                let (amount, comment) = match arg.split_once('\u{FF}') {
                    Some((amount, comment)) => (amount.to_string(), comment.to_string()),
                    None => (arg.to_string(), String::new()),
                };
                let sender_user_id = if sender_user_id
                    == crate::user_manager::UserManager::get_service_notifications_user_id()
                {
                    UserId::default()
                } else {
                    sender_user_id
                };
                return td_api::PushMessageContent::pushMessageContentTonWalletTransfer(
                    td_api::pushMessageContentTonWalletTransfer {
                        sender_user_id: sender_user_id.get(),
                        amount,
                        comment,
                    },
                );
            }
        }
        b'I' if key == "MESSAGE_INVOICE" => {
            return td_api::PushMessageContent::pushMessageContentInvoice(
                td_api::pushMessageContentInvoice {
                    price: arg.to_string(),
                    is_pinned,
                },
            );
        }
        b'L' => {
            if key == "MESSAGE_LIVE_LOCATION" {
                return td_api::PushMessageContent::pushMessageContentLocation(
                    td_api::pushMessageContentLocation {
                        is_live: false,
                        is_pinned,
                    },
                );
            }
            if key == "MESSAGE_LOCATION" {
                return td_api::PushMessageContent::pushMessageContentLocation(
                    td_api::pushMessageContentLocation {
                        is_live: true,
                        is_pinned,
                    },
                );
            }
        }
        b'P' => {
            if key == "MESSAGE_PAID_MEDIA" {
                let star_count = if is_pinned { 0 } else { to_integer_i64(arg) };
                return td_api::PushMessageContent::pushMessageContentPaidMedia(
                    td_api::pushMessageContentPaidMedia {
                        star_count,
                        is_pinned,
                    },
                );
            }
            if key == "MESSAGE_PHOTO" {
                return td_api::PushMessageContent::pushMessageContentPhoto(
                    td_api::pushMessageContentPhoto {
                        photo: env.get_photo_object(photo),
                        caption: arg.to_string(),
                        is_secret: false,
                        is_pinned,
                    },
                );
            }
            if key == "MESSAGE_PHOTOS" {
                return td_api::PushMessageContent::pushMessageContentMediaAlbum(
                    td_api::pushMessageContentMediaAlbum {
                        total_count: to_integer_i32(arg),
                        has_photos: true,
                        has_videos: false,
                        has_audios: false,
                        has_documents: false,
                    },
                );
            }
            if key == "MESSAGE_POLL" {
                return td_api::PushMessageContent::pushMessageContentPoll(
                    td_api::pushMessageContentPoll {
                        question: arg.to_string(),
                        is_regular: true,
                        is_pinned,
                    },
                );
            }
            if key == "MESSAGE_POLL_APPEND" {
                return td_api::PushMessageContent::pushMessageContentPollOptionAdded(
                    td_api::pushMessageContentPollOptionAdded {
                        text: arg.to_string(),
                    },
                );
            }
            if key == "MESSAGE_PROXIMITY" {
                return td_api::PushMessageContent::pushMessageContentProximityAlertTriggered(
                    td_api::pushMessageContentProximityAlertTriggered {
                        distance: to_integer_i32(arg),
                    },
                );
            }
        }
        b'Q' if key == "MESSAGE_QUIZ" => {
            return td_api::PushMessageContent::pushMessageContentPoll(
                td_api::pushMessageContentPoll {
                    question: arg.to_string(),
                    is_regular: false,
                    is_pinned,
                },
            );
        }
        b'R' if key == "MESSAGE_RECURRING_PAYMENT" => {
            return td_api::PushMessageContent::pushMessageContentRecurringPayment(
                td_api::pushMessageContentRecurringPayment {
                    amount: arg.to_string(),
                },
            );
        }
        b'S' => {
            if key == "MESSAGE_SAME_WALLPAPER" {
                return td_api::PushMessageContent::pushMessageContentChatSetBackground(
                    td_api::pushMessageContentChatSetBackground { is_same: true },
                );
            }
            if key == "MESSAGE_SCREENSHOT_TAKEN" {
                return td_api::PushMessageContent::pushMessageContentScreenshotTaken(
                    td_api::pushMessageContentScreenshotTaken,
                );
            }
            if key == "MESSAGE_SECRET_PHOTO" {
                return td_api::PushMessageContent::pushMessageContentPhoto(
                    td_api::pushMessageContentPhoto {
                        photo: None,
                        caption: arg.to_string(),
                        is_secret: true,
                        is_pinned: false,
                    },
                );
            }
            if key == "MESSAGE_SECRET_VIDEO" {
                return td_api::PushMessageContent::pushMessageContentVideo(
                    td_api::pushMessageContentVideo {
                        video: None,
                        caption: arg.to_string(),
                        is_secret: true,
                        is_pinned: false,
                    },
                );
            }
            if key == "MESSAGE_STARGIFT" {
                return td_api::PushMessageContent::pushMessageContentGift(
                    td_api::pushMessageContentGift {
                        star_count: to_integer_i64(arg),
                        is_prepaid_upgrade: false,
                    },
                );
            }
            if key == "MESSAGE_STARGIFT_PREPAID_UPGRADE" {
                return td_api::PushMessageContent::pushMessageContentGift(
                    td_api::pushMessageContentGift {
                        star_count: to_integer_i64(arg),
                        is_prepaid_upgrade: true,
                    },
                );
            }
            if key == "MESSAGE_STARGIFT_TRANSFER" {
                return td_api::PushMessageContent::pushMessageContentUpgradedGift(
                    td_api::pushMessageContentUpgradedGift {
                        is_upgrade: false,
                        is_prepaid_upgrade: false,
                    },
                );
            }
            if key == "MESSAGE_STARGIFT_UPGRADE" {
                return td_api::PushMessageContent::pushMessageContentUpgradedGift(
                    td_api::pushMessageContentUpgradedGift {
                        is_upgrade: true,
                        is_prepaid_upgrade: false,
                    },
                );
            }
            if key == "MESSAGE_STARGIFT_UNPACK_UPGRADE" {
                return td_api::PushMessageContent::pushMessageContentUpgradedGift(
                    td_api::pushMessageContentUpgradedGift {
                        is_upgrade: false,
                        is_prepaid_upgrade: true,
                    },
                );
            }
            if key == "MESSAGE_STICKER" {
                return td_api::PushMessageContent::pushMessageContentSticker(
                    td_api::pushMessageContentSticker {
                        sticker: env.get_sticker_object(document.file_id),
                        emoji: trim_ascii(arg).to_string(),
                        is_pinned,
                    },
                );
            }
            if key == "MESSAGE_STORY" {
                return td_api::PushMessageContent::pushMessageContentStory(
                    td_api::pushMessageContentStory {
                        is_mention: false,
                        is_pinned,
                    },
                );
            }
            if key == "MESSAGE_STORY_MENTION" {
                return td_api::PushMessageContent::pushMessageContentStory(
                    td_api::pushMessageContentStory {
                        is_mention: true,
                        is_pinned,
                    },
                );
            }
            if key == "MESSAGE_SUGGEST_BIRTHDAY" {
                return td_api::PushMessageContent::pushMessageContentSuggestBirthdate(
                    td_api::pushMessageContentSuggestBirthdate,
                );
            }
            if key == "MESSAGE_SUGGEST_PHOTO" {
                return td_api::PushMessageContent::pushMessageContentSuggestProfilePhoto(
                    td_api::pushMessageContentSuggestProfilePhoto,
                );
            }
        }
        b'T' => {
            if key == "MESSAGE_TEXT" {
                return td_api::PushMessageContent::pushMessageContentText(
                    td_api::pushMessageContentText {
                        text: arg.to_string(),
                        is_pinned,
                    },
                );
            }
            if key == "MESSAGE_TODO" {
                return td_api::PushMessageContent::pushMessageContentChecklist(
                    td_api::pushMessageContentChecklist {
                        title: arg.to_string(),
                        is_pinned,
                    },
                );
            }
            if key == "MESSAGE_TODO_APPEND" {
                return td_api::PushMessageContent::pushMessageContentChecklistTasksAdded(
                    td_api::pushMessageContentChecklistTasksAdded {
                        task_count: to_integer_i32(arg),
                    },
                );
            }
            if key == "MESSAGE_TODO_DONE" {
                return td_api::PushMessageContent::pushMessageContentChecklistTasksDone(
                    td_api::pushMessageContentChecklistTasksDone {
                        task_count: to_integer_i32(arg),
                    },
                );
            }
        }
        b'V' => {
            if key == "MESSAGE_VIDEO" {
                return td_api::PushMessageContent::pushMessageContentVideo(
                    td_api::pushMessageContentVideo {
                        video: env.get_video_object(document.file_id),
                        caption: arg.to_string(),
                        is_secret: false,
                        is_pinned,
                    },
                );
            }
            if key == "MESSAGE_VIDEO_NOTE" {
                return td_api::PushMessageContent::pushMessageContentVideoNote(
                    td_api::pushMessageContentVideoNote {
                        video_note: env.get_video_note_object(document.file_id),
                        is_pinned,
                    },
                );
            }
            if key == "MESSAGE_VIDEOS" {
                return td_api::PushMessageContent::pushMessageContentMediaAlbum(
                    td_api::pushMessageContentMediaAlbum {
                        total_count: to_integer_i32(arg),
                        has_photos: false,
                        has_videos: true,
                        has_audios: false,
                        has_documents: false,
                    },
                );
            }
            if key == "MESSAGE_VOICE_NOTE" {
                return td_api::PushMessageContent::pushMessageContentVoiceNote(
                    td_api::pushMessageContentVoiceNote {
                        voice_note: env.get_voice_note_object(document.file_id),
                        is_pinned,
                    },
                );
            }
        }
        b'W' => {
            if key == "MESSAGE_WALLET_TONCONNECT_REQUEST" {
                return td_api::PushMessageContent::pushMessageContentTonConnectRequest(
                    td_api::pushMessageContentTonConnectRequest {
                        dapp_name: arg.to_string(),
                    },
                );
            }
            if key == "MESSAGE_WALLPAPER" {
                return td_api::PushMessageContent::pushMessageContentChatSetBackground(
                    td_api::pushMessageContentChatSetBackground { is_same: false },
                );
            }
        }
        _ => {}
    }
    panic!("Have unsupported push notification key {key}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip() {
        let mut bytes = Vec::new();
        NotificationId::new(123).store(&mut bytes);
        assert_eq!(bytes, 123i32.to_le_bytes());
        assert_eq!(NotificationId::parse_value(&bytes).unwrap().get(), 123);

        let mut bytes = Vec::new();
        NotificationGroupId::new(7).store(&mut bytes);
        assert_eq!(bytes, 7i32.to_le_bytes());
        assert_eq!(NotificationGroupId::parse_value(&bytes).unwrap().get(), 7);

        let mut bytes = Vec::new();
        NotificationObjectId::new(456).store(&mut bytes);
        assert_eq!(bytes, 456i64.to_le_bytes());
        assert_eq!(
            NotificationObjectId::parse_value(&bytes).unwrap().get(),
            456
        );

        assert!(!NotificationId::new(0).is_valid());
        assert!(NotificationId::new(1).is_valid());
        assert!(!NotificationGroupId::new(0).is_valid());
        assert!(NotificationGroupId::new(1).is_valid());
        assert!(!NotificationObjectId::new(0).is_valid());
        assert!(NotificationObjectId::new(1).is_valid());
    }

    #[test]
    fn group_key_ordering() {
        // most recent date first, then dialog id, then group id — all descending
        let a = NotificationGroupKey::new(NotificationGroupId::new(1), DialogId::new(5), 100);
        let b = NotificationGroupKey::new(NotificationGroupId::new(2), DialogId::new(9), 50);
        assert!(a < b);
        assert!((b >= a));
        let c = NotificationGroupKey::new(NotificationGroupId::new(1), DialogId::new(9), 100);
        assert!(c < a);
        let d = NotificationGroupKey::new(NotificationGroupId::new(2), DialogId::new(9), 100);
        assert!(d < c);
        let e = NotificationGroupKey::new(NotificationGroupId::new(2), DialogId::new(5), 100);
        // equal date and dialog: the larger group id sorts first
        assert!(e < a);
        // equal date: the larger dialog id sorts first
        assert!(d < e);
    }

    #[test]
    fn group_info_round_trip() {
        let mut info = NotificationGroupInfo::with_group_id(NotificationGroupId::new(11));
        assert!(info.is_valid());
        assert!(info.is_active());
        assert!(info.set_last_notification(100, NotificationId::new(42)));
        assert!(!info.set_last_notification(100, NotificationId::new(42)));
        assert_eq!(info.get_last_notification_id(), NotificationId::new(42));
        assert!(info.set_max_removed_notification_id(
            NotificationId::new(7),
            NotificationObjectId::new(70)
        ));
        // the last notification survives — 42 > 7
        assert_eq!(info.get_last_notification_id(), NotificationId::new(42));
        assert!(info.is_removed_notification_id(NotificationId::new(7)));
        assert!(!info.is_removed_notification_id(NotificationId::new(8)));
        assert!(info.is_removed_object_id(NotificationObjectId::new(70)));
        assert!(info.is_used_notification_id(NotificationId::new(8)));
        assert!(!info.is_used_notification_id(NotificationId::new(43)));

        // a removed watermark above the last id resets it
        assert!(info.set_max_removed_notification_id(
            NotificationId::new(43),
            NotificationObjectId::default()
        ));
        assert_eq!(info.get_last_notification_id(), NotificationId::default());

        // store/parse round trip
        let mut bytes = Vec::new();
        info.store(&mut bytes);
        let parsed = NotificationGroupInfo::parse_value(
            &bytes,
            ADD_NOTIFICATION_GROUP_INFO_MAX_REMOVED_MESSAGE_ID,
        )
        .unwrap();
        assert!(parsed.is_valid());
        assert_eq!(parsed.get_last_notification_id(), NotificationId::default());
        assert!(parsed.is_removed_notification_id(NotificationId::new(43)));
        assert!(parsed.is_removed_object_id(NotificationObjectId::new(70)));
    }

    #[test]
    fn group_info_reuse() {
        let mut info = NotificationGroupInfo::with_group_id(NotificationGroupId::new(5));
        let mut keys = Vec::new();
        info.add_group_key_if_changed(&mut keys, DialogId::new(10));
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].group_id, NotificationGroupId::new(5));
        assert_eq!(keys[0].dialog_id, DialogId::new(10));
        assert_eq!(keys[0].last_notification_date, 0);

        info.try_reuse();
        info.add_group_key_if_changed(&mut keys, DialogId::new(10));
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[1].dialog_id, DialogId::default());
        assert_eq!(info.get_reused_group_id(), NotificationGroupId::new(5));
        assert!(!info.is_valid());
    }

    #[test]
    fn notification_type_dispatch() {
        let delayed = NotificationType::new_message(MessageId::new(1 << 20), true);
        assert!(delayed.can_be_delayed());
        assert!(!delayed.is_temporary());
        assert_eq!(
            delayed.get_object_id(),
            NotificationObjectId::from_message_id(MessageId::new(1 << 20))
        );

        let local = NotificationType::new_message(MessageId::new((1 << 20) | 1), false);
        assert!(!local.can_be_delayed());

        assert!(!NotificationType::new_secret_chat().can_be_delayed());
        assert!(!NotificationType::new_call(CallId::new(1)).can_be_delayed());
        assert_eq!(
            NotificationType::new_call(CallId::new(1)).get_object_id(),
            NotificationObjectId::max()
        );

        let push = NotificationType::new_push_message(
            UserId::new(1),
            DialogId::default(),
            "name".to_string(),
            false,
            MessageId::new(2),
            "MESSAGE_TEXT".to_string(),
            "text".to_string(),
            Photo::default(),
            Document::default(),
        );
        assert!(push.is_temporary());
        assert!(!push.can_be_delayed());
        assert_eq!(
            push.get_object_id(),
            NotificationObjectId::from_message_id(MessageId::new(2))
        );
        assert!(push.get_file_ids().is_empty());
    }

    struct TestEnv;

    impl NotificationTypeEnv for TestEnv {
        fn get_message_object(
            &self,
            _dialog_id: DialogId,
            message_id: MessageId,
        ) -> Option<td_api::Message> {
            if message_id == MessageId::new(100) {
                None
            } else {
                Some(td_api::Message::default())
            }
        }
    }

    #[test]
    fn notification_object_skips_unloaded_messages() {
        let loaded = Notification::new(
            NotificationId::new(1),
            100,
            false,
            NotificationType::new_message(MessageId::new(1), true),
        );
        let object = get_notification_object(&TestEnv, DialogId::new(3), &loaded);
        assert!(object.r#type.is_some());
        assert_eq!(object.id, 1);
        assert_eq!(object.date, 100);
        assert!(!object.is_silent);

        let unloaded = Notification::new(
            NotificationId::new(2),
            100,
            true,
            NotificationType::new_message(MessageId::new(100), true),
        );
        let object = get_notification_object(&TestEnv, DialogId::new(3), &unloaded);
        assert!(object.r#type.is_none());
        assert!(object.is_silent);
    }

    #[test]
    fn push_content_mapping() {
        use td_api::PushMessageContent::*;

        let empty_photo = Photo::default();
        let document = Document::default();

        let content = get_push_message_content_object(
            &TestEnv,
            "MESSAGE",
            "",
            UserId::default(),
            &empty_photo,
            &document,
        );
        assert!(matches!(content, pushMessageContentHidden(_)));

        let content = get_push_message_content_object(
            &TestEnv,
            "PINNED_MESSAGE_TEXT",
            "hi",
            UserId::default(),
            &empty_photo,
            &document,
        );
        match content {
            pushMessageContentText(t) => {
                assert_eq!(t.text, "hi");
                assert!(t.is_pinned);
            }
            other => panic!("unexpected content {other:?}"),
        }

        let content = get_push_message_content_object(
            &TestEnv,
            "MESSAGE_TEXT",
            "hi",
            UserId::default(),
            &empty_photo,
            &document,
        );
        match content {
            pushMessageContentText(t) => {
                assert!(!t.is_pinned);
            }
            other => panic!("unexpected content {other:?}"),
        }

        // split-based keys
        let content = get_push_message_content_object(
            &TestEnv,
            "MESSAGE_GAME_SCORE",
            "42 win",
            UserId::default(),
            &empty_photo,
            &document,
        );
        match content {
            pushMessageContentGameScore(g) => {
                assert_eq!(g.score, 42);
                assert_eq!(g.title, "win");
                assert!(!g.is_pinned);
            }
            other => panic!("unexpected content {other:?}"),
        }

        // pinned game score keeps the arg as the title
        let content = get_push_message_content_object(
            &TestEnv,
            "PINNED_MESSAGE_GAME_SCORE",
            "42 win",
            UserId::default(),
            &empty_photo,
            &document,
        );
        match content {
            pushMessageContentGameScore(g) => {
                assert_eq!(g.score, 0);
                assert_eq!(g.title, "");
                assert!(g.is_pinned);
            }
            other => panic!("unexpected content {other:?}"),
        }

        let content = get_push_message_content_object(
            &TestEnv,
            "MESSAGE_GIVEAWAY",
            "10 3",
            UserId::default(),
            &empty_photo,
            &document,
        );
        match content {
            pushMessageContentGiveaway(g) => {
                assert_eq!(g.winner_count, 10);
                assert!(matches!(
                    g.prize,
                    Some(td_api::GiveawayPrize::giveawayPrizePremium(
                        td_api::giveawayPrizePremium { month_count: 3 }
                    ))
                ));
                assert!(!g.is_pinned);
            }
            other => panic!("unexpected content {other:?}"),
        }

        let content = get_push_message_content_object(
            &TestEnv,
            "MESSAGE_GIVEAWAY_STARS",
            "2 500",
            UserId::default(),
            &empty_photo,
            &document,
        );
        match content {
            pushMessageContentGiveaway(g) => {
                assert_eq!(g.winner_count, 2);
                assert!(matches!(
                    g.prize,
                    Some(td_api::GiveawayPrize::giveawayPrizeStars(
                        td_api::giveawayPrizeStars { star_count: 500 }
                    ))
                ));
            }
            other => panic!("unexpected content {other:?}"),
        }

        // secret media have no photo/video objects
        let content = get_push_message_content_object(
            &TestEnv,
            "MESSAGE_SECRET_PHOTO",
            "cap",
            UserId::default(),
            &empty_photo,
            &document,
        );
        match content {
            pushMessageContentPhoto(p) => {
                assert!(p.photo.is_none());
                assert!(p.is_secret);
                assert!(!p.is_pinned);
                assert_eq!(p.caption, "cap");
            }
            other => panic!("unexpected content {other:?}"),
        }

        // sticker trims the emoji arg
        let content = get_push_message_content_object(
            &TestEnv,
            "MESSAGE_STICKER",
            "  emoji  ",
            UserId::default(),
            &empty_photo,
            &document,
        );
        match content {
            pushMessageContentSticker(s) => {
                assert_eq!(s.emoji, "emoji");
                assert!(s.sticker.is_none()); // seam
            }
            other => panic!("unexpected content {other:?}"),
        }
    }

    #[test]
    fn to_integer_lenient_parse() {
        assert_eq!(to_integer_i32(""), 0);
        assert_eq!(to_integer_i32("42"), 42);
        assert_eq!(to_integer_i32("-7"), -7);
        assert_eq!(to_integer_i32("123abc"), 123);
        assert_eq!(to_integer_i32("abc"), 0);
        assert_eq!(to_integer_i32("-"), 0);
        assert_eq!(to_integer_i32(&i32::MAX.to_string()), i32::MAX);
        assert_eq!(to_integer_i32("2147483648"), i32::MIN);
        assert_eq!(to_integer_i32("-2147483648"), i32::MIN);
        assert_eq!(to_integer_i64("9223372036854775808"), i64::MIN);
    }

    #[test]
    fn trim_ascii_behavior() {
        assert_eq!(trim_ascii("  a b  "), "a b");
        assert_eq!(trim_ascii("\ta\n"), "a");
        assert_eq!(trim_ascii("abc"), "abc");
        assert_eq!(trim_ascii(""), "");
    }

    #[test]
    fn split_first_space_behavior() {
        assert_eq!(split_first_space("a b c"), ("a", "b c"));
        assert_eq!(split_first_space("abc"), ("abc", ""));
        assert_eq!(split_first_space(""), ("", ""));
    }
}
