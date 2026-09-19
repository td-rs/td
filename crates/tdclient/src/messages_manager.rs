//! MessagesManager foundations (M.10.1) — the message/dialog identifier
//! spaces and the per-dialog notification-group bookkeeping, ported from
//! tdlib 1.8.67 (MessageId.{h,cpp}, ServerMessageId.h,
//! ScheduledServerMessageId.h, EphemeralMessageId.h, MessageFullId.h,
//! DialogId.{h,cpp}, NotificationGroupId.h, NotificationId.h,
//! NotificationObjectId.h, NotificationGroupKey.h,
//! NotificationGroupInfo.{h,hpp}, InputDialogId.{h,cpp}).
//!
//! These are the value types the whole send/receive/history machinery is
//! built on; `Message`/`Dialog` structs and the manager itself follow in the
//! later M.10.1 tasks.
//!
//! Divergences (documented collapses):
//! - `chat_manager.rs` carries a bare `MessageId` stand-in from M.5; the full
//!   type lives here and the stand-in is superseded at the M.5/M.10
//!   integration pass (same policy for the M.4.15 `message_id.rs` subset).
//! - C++ `CHECK` on impossible states stays `assert!` (release-fatal), except
//!   inside derived ordering impls where `debug_assert!` is used; cross-kind
//!   (`scheduled` vs ordinary) `MessageId` comparison is a caller bug, as in
//!   the C++ `operator<`.
//! - `VLOG(notifications)` traces are dropped; `LOG(ERROR)` warnings are kept
//!   via `log_error!`.

use crate::chat_manager::{ChannelId, ChannelType, ChatId};
use crate::message_content::{
    BackgroundInfo, InputGroupCallId, MessageContent, MessageContentType, SuggestedPostPrice,
    can_be_secret_message_content,
};
use crate::message_db::MessageDbAsyncInterface;
use crate::message_entity::FormattedText;
use crate::restriction_reason::RestrictionReason;
use crate::store_parse::{Parser, store_flags, store_i32, store_i64, store_string};
use crate::td_api;
use crate::user_manager::{FileId, SecretChatId, UserId};
use std::collections::{HashMap, HashSet};
use tdmtproto::telegram_api;
use tdutils::hash::{combine_hashes, hash_int64, hash_uint32};
use tdutils::log_error;
use tdutils::status::Status;

/// `td::MessageType` (MessageId.h:22) — the discriminant order is load-bearing:
/// `MessageId::get_next_message_id` compares the numeric values.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(i32)]
pub enum MessageType {
    #[default]
    None = 0,
    Server = 1,
    YetUnsent = 2,
    Local = 3,
}

/// `td::ServerMessageId` (ServerMessageId.h) — the raw server-side id.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct ServerMessageId(i32);

impl ServerMessageId {
    pub fn new(id: i32) -> ServerMessageId {
        ServerMessageId(id)
    }

    pub fn is_valid(self) -> bool {
        self.0 > 0
    }

    pub fn get(self) -> i32 {
        self.0
    }
}

/// `td::ScheduledServerMessageId` (ScheduledServerMessageId.h) — the server id
/// inside the scheduled message id space (`0 < id < 2^18`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct ScheduledServerMessageId(i32);

impl ScheduledServerMessageId {
    pub fn new(id: i32) -> ScheduledServerMessageId {
        ScheduledServerMessageId(id)
    }

    pub fn is_valid(self) -> bool {
        self.0 > 0 && self.0 < (1 << 18)
    }

    pub fn get(self) -> i32 {
        self.0
    }

    pub fn store(self, out: &mut Vec<u8>) {
        store_i32(out, self.0);
    }

    pub fn parse_value(input: &[u8]) -> Result<ScheduledServerMessageId, &'static str> {
        let mut parser = Parser::new(input);
        let id = parser.i32_()?;
        parser.fetch_end()?;
        Ok(ScheduledServerMessageId(id))
    }
}

/// `td::EphemeralMessageId` (EphemeralMessageId.h).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EphemeralMessageId(i32);

impl EphemeralMessageId {
    pub fn new(id: i32) -> EphemeralMessageId {
        EphemeralMessageId(id)
    }

    pub fn is_valid(self) -> bool {
        self.0 > 0
    }

    pub fn get(self) -> i32 {
        self.0
    }

    /// `EphemeralMessageId::get_ephemeral_message_ids` (EphemeralMessageId.h:36-43).
    pub fn get_ephemeral_message_ids(message_ids: &[i32]) -> Vec<EphemeralMessageId> {
        message_ids
            .iter()
            .map(|&id| EphemeralMessageId(id))
            .collect()
    }

    pub fn store(self, out: &mut Vec<u8>) {
        store_i32(out, self.0);
    }

    pub fn parse_value(input: &[u8]) -> Result<EphemeralMessageId, &'static str> {
        let mut parser = Parser::new(input);
        let id = parser.i32_()?;
        parser.fetch_end()?;
        Ok(EphemeralMessageId(id))
    }

    /// The `ParserT` overload of `EphemeralMessageId::parse`
    /// (EphemeralMessageId.hpp).
    pub fn parse_from(parser: &mut Parser) -> Result<EphemeralMessageId, &'static str> {
        Ok(EphemeralMessageId(parser.i32_()?))
    }
}

/// `td::MessageId` (MessageId.h) — the full three-id-space identifier.
///
/// Ordinary id layout: `|server_message_id:31|local_id:17|0|type:2|`; the
/// scheduled layout reuses bit 2 as the scheduled marker and bits 3..21 for
/// the scheduled server id, with bits 21.. holding `send_date - 2^30`.
/// Sponsored ids are local ids above `max()` in their own band.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct MessageId(i64);

impl MessageId {
    pub const SERVER_ID_SHIFT: i32 = 20;
    pub const SHORT_TYPE_MASK: i32 = (1 << 2) - 1;
    pub const TYPE_MASK: i32 = (1 << 3) - 1;
    pub const FULL_TYPE_MASK: i32 = (1 << MessageId::SERVER_ID_SHIFT) - 1;
    pub const SCHEDULED_MASK: i32 = 4;
    pub const TYPE_YET_UNSENT: i32 = 1;
    pub const TYPE_LOCAL: i32 = 2;

    pub fn new(id: i64) -> MessageId {
        MessageId(id)
    }

    /// `MessageId(ServerMessageId)` (MessageId.h:59-61).
    pub fn from_server_message_id(server_message_id: ServerMessageId) -> MessageId {
        MessageId((server_message_id.get() as i64) << MessageId::SERVER_ID_SHIFT)
    }

    /// `MessageId(ScheduledServerMessageId, send_date, force)`
    /// (MessageId.cpp:15-26). The C++ constructor logs and falls back to the
    /// default id on a past date or invalid server id.
    pub fn from_scheduled(
        server_message_id: ScheduledServerMessageId,
        send_date: i32,
        force: bool,
    ) -> MessageId {
        if send_date <= (1 << 30) {
            log_error!("Scheduled message send date {} is in the past", send_date);
            return MessageId::default();
        }
        if !server_message_id.is_valid() && !force {
            log_error!(
                "Scheduled message ID {} is invalid",
                server_message_id.get()
            );
            return MessageId::default();
        }
        MessageId(
            ((send_date - (1 << 30)) as i64) << 21
                | (server_message_id.get() as i64) << 3
                | i64::from(MessageId::SCHEDULED_MASK),
        )
    }

    /// `MessageId::min` (MessageId.h:70-72) — the smallest yet-unsent id.
    pub fn min() -> MessageId {
        MessageId(i64::from(MessageId::TYPE_YET_UNSENT))
    }

    /// `MessageId::max` (MessageId.h:73-75).
    pub fn max() -> MessageId {
        MessageId(i64::from(i32::MAX) << MessageId::SERVER_ID_SHIFT)
    }

    /// The `max()` value as a constant, for bounds like the database
    /// `from_message_id` clamps.
    pub const MAX: MessageId = MessageId((i32::MAX as i64) << MessageId::SERVER_ID_SHIFT);

    /// `MessageId::get_message_id` (MessageId.cpp:28-49).
    pub fn get_message_id(message: &telegram_api::Message, is_scheduled: bool) -> MessageId {
        match message {
            telegram_api::Message::messageEmpty(m) => {
                if is_scheduled {
                    MessageId::default()
                } else {
                    MessageId::from_server_message_id(ServerMessageId::new(m.id))
                }
            }
            telegram_api::Message::message(m) => {
                if is_scheduled {
                    MessageId::from_scheduled(ScheduledServerMessageId::new(m.id), m.date, false)
                } else {
                    MessageId::from_server_message_id(ServerMessageId::new(m.id))
                }
            }
            telegram_api::Message::messageService(m) => {
                if is_scheduled {
                    MessageId::from_scheduled(ScheduledServerMessageId::new(m.id), m.date, false)
                } else {
                    MessageId::from_server_message_id(ServerMessageId::new(m.id))
                }
            }
        }
    }

    /// `MessageId::get_max_message_id` (MessageId.cpp:55-64).
    pub fn get_max_message_id(messages: &[telegram_api::Message]) -> MessageId {
        let mut max_message_id = MessageId::default();
        for message in messages {
            let message_id = MessageId::get_message_id(message, false);
            if message_id > max_message_id {
                max_message_id = message_id;
            }
        }
        max_message_id
    }

    /// `MessageId::is_message_id_order_ascending` (MessageId.cpp:66-82); the
    /// C++ error log dumps every received message, here only the ids.
    pub fn is_message_id_order_ascending(messages: &[telegram_api::Message], source: &str) -> bool {
        let mut cur_message_id = MessageId::default();
        for message in messages {
            let message_id = MessageId::get_message_id(message, false);
            if message_id <= cur_message_id || !message_id.is_valid() {
                log_error!(
                    "Receive {} after {} from {}",
                    DisplayMessageId(message_id),
                    DisplayMessageId(cur_message_id),
                    source
                );
                return false;
            }
            cur_message_id = message_id;
        }
        true
    }

    /// `MessageId::is_message_id_order_descending` (MessageId.cpp:84-100).
    pub fn is_message_id_order_descending(
        messages: &[telegram_api::Message],
        source: &str,
    ) -> bool {
        let mut cur_message_id = MessageId::max();
        for message in messages {
            let message_id = MessageId::get_message_id(message, false);
            if message_id >= cur_message_id || !message_id.is_valid() {
                log_error!(
                    "Receive {} after {} from {}",
                    DisplayMessageId(message_id),
                    DisplayMessageId(cur_message_id),
                    source
                );
                return false;
            }
            cur_message_id = message_id;
        }
        true
    }

    /// `MessageId::get_message_ids` (MessageId.cpp:102-104).
    pub fn get_message_ids(input_message_ids: &[i64]) -> Vec<MessageId> {
        input_message_ids
            .iter()
            .map(|&id| MessageId::new(id))
            .collect()
    }

    /// `MessageId::get_server_message_ids` (MessageId.cpp:106-108).
    pub fn get_server_message_ids(message_ids: &[MessageId]) -> Vec<i32> {
        message_ids
            .iter()
            .map(|&id| id.get_server_message_id().get())
            .collect()
    }

    /// `MessageId::get_scheduled_server_message_ids` (MessageId.cpp:110-113).
    pub fn get_scheduled_server_message_ids(message_ids: &[MessageId]) -> Vec<i32> {
        message_ids
            .iter()
            .map(|&id| id.get_scheduled_server_message_id().get())
            .collect()
    }

    pub fn get(self) -> i64 {
        self.0
    }

    /// `MessageId::is_valid` (MessageId.cpp:115-124).
    pub fn is_valid(self) -> bool {
        if self.0 <= 0 || self.0 > MessageId::max().get() {
            return false;
        }
        if self.0 & i64::from(MessageId::FULL_TYPE_MASK) == 0 {
            return true;
        }
        let ty = self.0 & i64::from(MessageId::TYPE_MASK);
        ty == i64::from(MessageId::TYPE_YET_UNSENT) || ty == i64::from(MessageId::TYPE_LOCAL)
    }

    /// `MessageId::is_valid_scheduled` (MessageId.cpp:126-132).
    pub fn is_valid_scheduled(self) -> bool {
        if self.0 <= 0 || self.0 > (1i64 << 51) {
            return false;
        }
        let ty = self.0 & i64::from(MessageId::TYPE_MASK);
        ty == i64::from(MessageId::SCHEDULED_MASK)
            || ty == i64::from(MessageId::SCHEDULED_MASK | MessageId::TYPE_YET_UNSENT)
            || ty == i64::from(MessageId::SCHEDULED_MASK | MessageId::TYPE_LOCAL)
    }

    /// `MessageId::is_valid_sponsored` (MessageId.cpp:134-140).
    pub fn is_valid_sponsored(self) -> bool {
        if self.0 <= MessageId::max().get() || self.0 > (1i64 << 51) {
            return false;
        }
        self.0 & i64::from(MessageId::TYPE_MASK) == i64::from(MessageId::TYPE_LOCAL)
    }

    /// `MessageId::get_type` (MessageId.cpp:142-171).
    pub fn get_type(self) -> MessageType {
        if self.0 <= 0 || self.0 > (1i64 << 51) {
            return MessageType::None;
        }

        if self.is_scheduled() {
            return match self.0 & i64::from(MessageId::TYPE_MASK) {
                x if x == i64::from(MessageId::SCHEDULED_MASK | MessageId::TYPE_YET_UNSENT) => {
                    MessageType::YetUnsent
                }
                x if x == i64::from(MessageId::SCHEDULED_MASK | MessageId::TYPE_LOCAL) => {
                    MessageType::Local
                }
                x if x == i64::from(MessageId::SCHEDULED_MASK) => MessageType::Server,
                _ => MessageType::None,
            };
        }

        if self.0 & i64::from(MessageId::FULL_TYPE_MASK) == 0 {
            return MessageType::Server;
        }
        match self.0 & i64::from(MessageId::TYPE_MASK) {
            x if x == i64::from(MessageId::TYPE_YET_UNSENT) => MessageType::YetUnsent,
            x if x == i64::from(MessageId::TYPE_LOCAL) => MessageType::Local,
            _ => MessageType::None,
        }
    }

    /// `MessageId::is_scheduled` (MessageId.h:107-109).
    pub fn is_scheduled(self) -> bool {
        self.0 & i64::from(MessageId::SCHEDULED_MASK) != 0
    }

    /// `MessageId::is_yet_unsent` (MessageId.h:111-114).
    pub fn is_yet_unsent(self) -> bool {
        assert!(self.is_valid() || self.is_scheduled());
        self.0 & i64::from(MessageId::SHORT_TYPE_MASK) == i64::from(MessageId::TYPE_YET_UNSENT)
    }

    /// `MessageId::is_local` (MessageId.h:116-119).
    pub fn is_local(self) -> bool {
        assert!(self.is_valid() || self.is_scheduled());
        self.0 & i64::from(MessageId::SHORT_TYPE_MASK) == i64::from(MessageId::TYPE_LOCAL)
    }

    /// `MessageId::is_server` (MessageId.h:121-124) — also checks validness.
    pub fn is_server(self) -> bool {
        self.0 & i64::from(MessageId::FULL_TYPE_MASK) == 0
            && self.0 > 0
            && self.0 <= MessageId::max().get()
    }

    /// `MessageId::is_scheduled_server` (MessageId.h:126-129).
    pub fn is_scheduled_server(self) -> bool {
        assert!(self.is_valid_scheduled());
        self.0 & i64::from(MessageId::SHORT_TYPE_MASK) == 0
    }

    /// `MessageId::is_any_server` (MessageId.h:131-133).
    pub fn is_any_server(self) -> bool {
        if self.is_scheduled() {
            self.is_scheduled_server()
        } else {
            self.is_server()
        }
    }

    /// `MessageId::get_server_message_id` (MessageId.h:135-138).
    pub fn get_server_message_id(self) -> ServerMessageId {
        assert!(self.0 == 0 || self.is_server());
        self.get_server_message_id_force()
    }

    /// `MessageId::get_prev_server_message_id` (MessageId.h:141-144) — the
    /// greatest server message identifier not bigger than this id.
    pub fn get_prev_server_message_id(self) -> MessageId {
        assert!(!self.is_scheduled());
        MessageId(self.0 & !i64::from(MessageId::FULL_TYPE_MASK))
    }

    /// `MessageId::get_next_server_message_id` (MessageId.h:146-150) — the
    /// smallest server message identifier not less than this id.
    pub fn get_next_server_message_id(self) -> MessageId {
        assert!(!self.is_scheduled());
        MessageId(
            (self.0 + i64::from(MessageId::FULL_TYPE_MASK)) & !i64::from(MessageId::FULL_TYPE_MASK),
        )
    }

    /// `MessageId::get_scheduled_server_message_id` (MessageId.h:154-157).
    pub fn get_scheduled_server_message_id(self) -> ScheduledServerMessageId {
        assert!(self.is_scheduled_server());
        self.get_scheduled_server_message_id_force()
    }

    /// `MessageId::get_scheduled_server_message_id_force` (MessageId.h:51-54).
    fn get_scheduled_server_message_id_force(self) -> ScheduledServerMessageId {
        assert!(self.is_scheduled());
        ScheduledServerMessageId::new(((self.0 >> 3) & ((1 << 18) - 1) as i64) as i32)
    }

    /// `MessageId::get_scheduled_message_date` (MessageId.h:159-162).
    pub fn get_scheduled_message_date(self) -> i32 {
        assert!(self.is_valid_scheduled());
        ((self.0 >> 21) + (1i64 << 30)) as i32
    }

    /// `MessageId::get_server_message_id_force` (MessageId.cpp:173-176).
    fn get_server_message_id_force(self) -> ServerMessageId {
        assert!(!self.is_scheduled());
        ServerMessageId::new((self.0 >> MessageId::SERVER_ID_SHIFT) as i32)
    }

    /// `MessageId::get_next_message_id` (MessageId.cpp:178-215).
    pub fn get_next_message_id(self, ty: MessageType) -> MessageId {
        if self.is_scheduled() {
            assert!(self.is_valid_scheduled());
            let current_type = self.get_type();
            if (current_type as i32) < (ty as i32) {
                return MessageId(self.0 - i64::from(current_type as i32) + i64::from(ty as i32));
            }
            let base_id = (self.0 & !i64::from(MessageId::TYPE_MASK))
                + i64::from(MessageId::TYPE_MASK)
                + 1
                + i64::from(MessageId::SCHEDULED_MASK);
            return match ty {
                MessageType::Server => MessageId(base_id),
                MessageType::YetUnsent => {
                    MessageId(base_id + i64::from(MessageId::TYPE_YET_UNSENT))
                }
                MessageType::Local => MessageId(base_id + i64::from(MessageId::TYPE_LOCAL)),
                MessageType::None => unreachable!(),
            };
        }

        match ty {
            MessageType::Server => {
                if self.is_server() {
                    MessageId::from_server_message_id(ServerMessageId::new(
                        self.get_server_message_id().get() + 1,
                    ))
                } else {
                    self.get_next_server_message_id()
                }
            }
            MessageType::YetUnsent => MessageId(
                ((self.0 + i64::from(MessageId::TYPE_MASK) + 1
                    - i64::from(MessageId::TYPE_YET_UNSENT))
                    & !i64::from(MessageId::TYPE_MASK))
                    + i64::from(MessageId::TYPE_YET_UNSENT),
            ),
            MessageType::Local => MessageId(
                ((self.0 + i64::from(MessageId::TYPE_MASK) + 1 - i64::from(MessageId::TYPE_LOCAL))
                    & !i64::from(MessageId::TYPE_MASK))
                    + i64::from(MessageId::TYPE_LOCAL),
            ),
            MessageType::None => unreachable!(),
        }
    }

    /// `MessageId::store` (MessageId.h:192-195).
    pub fn store(self, out: &mut Vec<u8>) {
        store_i64(out, self.0);
    }

    /// `MessageId::parse` (MessageId.h:197-200).
    pub fn parse_value(input: &[u8]) -> Result<MessageId, &'static str> {
        let mut parser = Parser::new(input);
        let id = parser.i64_()?;
        parser.fetch_end()?;
        Ok(MessageId(id))
    }

    /// `td::parse(MessageId, Parser)` — the parser form the database layers
    /// use (raw i64, no gate, like the C++ `parse`).
    pub fn parse_from(parser: &mut Parser) -> Result<MessageId, &'static str> {
        Ok(MessageId::new(parser.i64_()?))
    }
}

// The C++ relational operators (MessageId.h:172-190) CHECK that both sides
// agree on scheduledness; that stays a debug assert here, because a total
// order by the raw id keeps every same-kind ordering identical.
impl PartialOrd for MessageId {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for MessageId {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        debug_assert!(
            self.is_scheduled() == other.is_scheduled(),
            "comparison of scheduled and ordinary message ids"
        );
        self.0.cmp(&other.0)
    }
}

/// `operator<<(StringBuilder, MessageId)` (MessageId.cpp:217-251) — the debug
/// print lives in a wrapper so `{:?}` on the raw struct stays derived.
pub struct DisplayMessageId(pub MessageId);

impl std::fmt::Display for DisplayMessageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message_id = self.0;
        if message_id.is_scheduled() {
            write!(f, "scheduled ")?;

            if !message_id.is_valid_scheduled() {
                return write!(f, "invalid message {}", message_id.get());
            }
            if message_id.is_scheduled_server() {
                return write!(
                    f,
                    "server message {}",
                    message_id.get_scheduled_server_message_id_force().get()
                );
            }
            if message_id.is_local() {
                return write!(
                    f,
                    "local message {}",
                    message_id.get_scheduled_server_message_id_force().get()
                );
            }
            if message_id.is_yet_unsent() {
                return write!(
                    f,
                    "yet unsent message {}",
                    message_id.get_scheduled_server_message_id_force().get()
                );
            }
            return write!(f, "bugged message {}", message_id.get());
        }

        if !message_id.is_valid() {
            return write!(f, "invalid message {}", message_id.get());
        }
        if message_id.is_server() {
            return write!(
                f,
                "server message {}",
                message_id.get_server_message_id_force().get()
            );
        }
        if message_id.is_local() {
            return write!(
                f,
                "local message {}.{}",
                message_id.get_server_message_id_force().get(),
                message_id.get() & i64::from(MessageId::FULL_TYPE_MASK)
            );
        }
        if message_id.is_yet_unsent() {
            return write!(
                f,
                "yet unsent message {}.{}",
                message_id.get_server_message_id_force().get(),
                message_id.get() & i64::from(MessageId::FULL_TYPE_MASK)
            );
        }
        write!(f, "bugged message {}", message_id.get())
    }
}

/// `td::DialogType` (DialogId.h:22) — the discriminant order matches the C++
/// `enum class DialogType : int32`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(i32)]
pub enum DialogType {
    #[default]
    None = 0,
    User = 1,
    Chat = 2,
    Channel = 3,
    SecretChat = 4,
}

/// `td::DialogId` (DialogId.{h,cpp}) — the unified peer identifier.
///
/// The valid bands are continuous (the C++ static_asserts in
/// `DialogId::get_type`, DialogId.cpp:21-24, are pinned as consts below):
/// users `1..2^40-1`, basic chats `-1..-MAX_CHAT_ID`, channels
/// `ZERO_CHANNEL_ID - channel_id` including the monoforum band above the
/// secret-chat reservation, secret chats `ZERO_SECRET_CHAT_ID + secret_chat_id`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DialogId(i64);

impl DialogId {
    pub const ZERO_SECRET_CHAT_ID: i64 = -2_000_000_000_000;
    pub const ZERO_CHANNEL_ID: i64 = -1_000_000_000_000;

    pub fn new(id: i64) -> DialogId {
        DialogId(id)
    }

    /// `DialogId(UserId)` (DialogId.cpp:54-61).
    pub fn from_user_id(user_id: UserId) -> DialogId {
        if user_id.is_valid() {
            DialogId(user_id.get())
        } else {
            DialogId::default()
        }
    }

    /// `DialogId(ChatId)` (DialogId.cpp:63-70).
    pub fn from_chat_id(chat_id: ChatId) -> DialogId {
        if chat_id.is_valid() {
            DialogId(-chat_id.get())
        } else {
            DialogId::default()
        }
    }

    /// `DialogId(ChannelId)` (DialogId.cpp:72-79).
    pub fn from_channel_id(channel_id: ChannelId) -> DialogId {
        if channel_id.is_valid() {
            DialogId(Self::ZERO_CHANNEL_ID - channel_id.get())
        } else {
            DialogId::default()
        }
    }

    /// `DialogId(SecretChatId)` (DialogId.cpp:81-88).
    pub fn from_secret_chat_id(secret_chat_id: SecretChatId) -> DialogId {
        if secret_chat_id.is_valid() {
            DialogId(Self::ZERO_SECRET_CHAT_ID + i64::from(secret_chat_id.get()))
        } else {
            DialogId::default()
        }
    }

    /// `DialogId::get_peer_id` (DialogId.cpp:120-160); invalid component ids
    /// log and decode to the default dialog.
    pub fn from_peer(peer: &telegram_api::Peer) -> DialogId {
        match peer {
            telegram_api::Peer::peerUser(p) => {
                let user_id = UserId::new(p.user_id);
                if !user_id.is_valid() {
                    log_error!("Receive invalid user {}", user_id.get());
                    return DialogId::default();
                }
                DialogId(user_id.get())
            }
            telegram_api::Peer::peerChat(p) => {
                let chat_id = ChatId::new(p.chat_id);
                if !chat_id.is_valid() {
                    log_error!("Receive invalid chat {}", chat_id.get());
                    return DialogId::default();
                }
                DialogId(-chat_id.get())
            }
            telegram_api::Peer::peerChannel(p) => {
                let channel_id = ChannelId::new(p.channel_id);
                if !channel_id.is_valid() {
                    log_error!("Receive invalid channel {}", channel_id.get());
                    return DialogId::default();
                }
                DialogId(Self::ZERO_CHANNEL_ID - channel_id.get())
            }
        }
    }

    /// `DialogId::get_message_dialog_id` (DialogId.cpp:162-183).
    pub fn get_message_dialog_id(message: &telegram_api::Message) -> DialogId {
        match message {
            telegram_api::Message::messageEmpty(m) => {
                if m.flags & 1 == 0 {
                    DialogId::default()
                } else {
                    DialogId::from_peer(m.peer_id.as_ref().unwrap())
                }
            }
            telegram_api::Message::message(m) => DialogId::from_peer(m.peer_id.as_ref().unwrap()),
            telegram_api::Message::messageService(m) => {
                DialogId::from_peer(m.peer_id.as_ref().unwrap())
            }
        }
    }

    pub fn get(self) -> i64 {
        self.0
    }

    /// `DialogId::is_valid` (DialogId.cpp:17-19).
    pub fn is_valid(self) -> bool {
        self.get_type() != DialogType::None
    }

    /// `DialogId::get_type` (DialogId.cpp:21-47). The C++ static_asserts
    /// pinning band continuity are compiled as consts below.
    pub fn get_type(self) -> DialogType {
        if self.0 < 0 {
            if -ChatId::MAX_CHAT_ID <= self.0 {
                return DialogType::Chat;
            }
            if Self::ZERO_CHANNEL_ID - ChannelId::MAX_CHANNEL_ID <= self.0
                && self.0 != Self::ZERO_CHANNEL_ID
            {
                return DialogType::Channel;
            }
            if Self::ZERO_SECRET_CHAT_ID + i64::from(i32::MIN) <= self.0
                && self.0 != Self::ZERO_SECRET_CHAT_ID
            {
                return DialogType::SecretChat;
            }
            if Self::ZERO_CHANNEL_ID - ChannelId::MAX_MONOFORUM_CHANNEL_ID <= self.0 {
                return DialogType::Channel;
            }
        } else if 0 < self.0 && self.0 <= UserId::MAX_USER_ID {
            return DialogType::User;
        }
        DialogType::None
    }

    /// `DialogId::get_user_id` (DialogId.cpp:49-52).
    pub fn get_user_id(self) -> UserId {
        assert!(self.get_type() == DialogType::User);
        UserId::new(self.0)
    }

    /// `DialogId::get_chat_id` (DialogId.cpp:53-56).
    pub fn get_chat_id(self) -> ChatId {
        assert!(self.get_type() == DialogType::Chat);
        ChatId::new(-self.0)
    }

    /// `DialogId::get_channel_id` (DialogId.cpp:58-61).
    pub fn get_channel_id(self) -> ChannelId {
        assert!(self.get_type() == DialogType::Channel);
        ChannelId::new(Self::ZERO_CHANNEL_ID - self.0)
    }

    /// `DialogId::get_secret_chat_id` (DialogId.cpp:63-66).
    pub fn get_secret_chat_id(self) -> SecretChatId {
        assert!(self.get_type() == DialogType::SecretChat);
        SecretChatId::new((self.0 - Self::ZERO_SECRET_CHAT_ID) as i32)
    }

    /// `DialogId::store` (DialogId.h:89-91).
    pub fn store(self, out: &mut Vec<u8>) {
        store_i64(out, self.0);
    }

    /// `DialogId::parse` (DialogId.h:93-95).
    pub fn parse_value(input: &[u8]) -> Result<DialogId, &'static str> {
        let mut parser = Parser::new(input);
        let id = parser.i64_()?;
        parser.fetch_end()?;
        Ok(DialogId(id))
    }

    /// `td::parse(DialogId, Parser)` — the parser form the database layers
    /// use.
    pub fn parse_from(parser: &mut Parser) -> Result<DialogId, &'static str> {
        Ok(DialogId::new(parser.i64_()?))
    }
}

// The C++ band-continuity static_asserts (DialogId.cpp:21-24).
const _: () = assert!(DialogId::ZERO_CHANNEL_ID + 1 == -ChatId::MAX_CHAT_ID);
const _: () = assert!(
    DialogId::ZERO_SECRET_CHAT_ID + i32::MAX as i64 + 1
        == DialogId::ZERO_CHANNEL_ID - ChannelId::MAX_CHANNEL_ID
);
const _: () = assert!(
    DialogId::ZERO_CHANNEL_ID - ChannelId::MIN_MONOFORUM_CHANNEL_ID + 1
        == DialogId::ZERO_SECRET_CHAT_ID + i32::MIN as i64
);

impl std::fmt::Display for DialogId {
    /// `operator<<(StringBuilder, DialogId)` (DialogId.h:106-108).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "chat {}", self.0)
    }
}

/// `td::MessageFullId` (MessageFullId.h) — the (dialog, message) pair key.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MessageFullId {
    dialog_id: DialogId,
    message_id: MessageId,
}

impl MessageFullId {
    pub fn new(dialog_id: DialogId, message_id: MessageId) -> MessageFullId {
        MessageFullId {
            dialog_id,
            message_id,
        }
    }

    pub fn get_dialog_id(self) -> DialogId {
        self.dialog_id
    }

    pub fn get_message_id(self) -> MessageId {
        self.message_id
    }

    /// `MessageFullId::get_message_full_id` (MessageFullId.h:38-43).
    pub fn get_message_full_id(
        message: &telegram_api::Message,
        is_scheduled: bool,
    ) -> MessageFullId {
        MessageFullId::new(
            DialogId::get_message_dialog_id(message),
            MessageId::get_message_id(message, is_scheduled),
        )
    }

    /// `MessageFullId::store` (MessageFullId.h:45-48).
    pub fn store(self, out: &mut Vec<u8>) {
        self.dialog_id.store(out);
        self.message_id.store(out);
    }

    /// `MessageFullId::parse` (MessageFullId.h:50-54).
    pub fn parse_value(input: &[u8]) -> Result<MessageFullId, &'static str> {
        let mut parser = Parser::new(input);
        let dialog_id = parser.i64_()?;
        let message_id = parser.i64_()?;
        parser.fetch_end()?;
        Ok(MessageFullId::new(
            DialogId(dialog_id),
            MessageId(message_id),
        ))
    }

    /// `td::parse(MessageFullId, Parser)` — the parser form the database
    /// layers use (dialog then message, two i64s).
    pub fn parse_from(parser: &mut Parser) -> Result<MessageFullId, &'static str> {
        Ok(MessageFullId::new(
            DialogId::parse_from(parser)?,
            MessageId::parse_from(parser)?,
        ))
    }
}

/// `td::QuickReplyShortcutId` (QuickReplyShortcutId.h).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct QuickReplyShortcutId(i32);

impl QuickReplyShortcutId {
    pub const MAX_SERVER_SHORTCUT_ID: i32 = 1999999999;

    pub fn new(id: i32) -> QuickReplyShortcutId {
        QuickReplyShortcutId(id)
    }

    pub fn get(self) -> i32 {
        self.0
    }

    /// `QuickReplyShortcutId::get_input_quick_reply_shortcut_ids`
    /// (QuickReplyShortcutId.h:34-43).
    pub fn get_input_quick_reply_shortcut_ids(
        quick_reply_shortcut_ids: &[QuickReplyShortcutId],
    ) -> Vec<i32> {
        quick_reply_shortcut_ids
            .iter()
            .map(|&id| id.get())
            .collect()
    }

    /// `QuickReplyShortcutId::get_quick_reply_shortcut_ids`
    /// (QuickReplyShortcutId.h:45-53).
    pub fn get_quick_reply_shortcut_ids(shortcut_ids: &[i32]) -> Vec<QuickReplyShortcutId> {
        shortcut_ids
            .iter()
            .map(|&id| QuickReplyShortcutId(id))
            .collect()
    }

    /// `QuickReplyShortcutId::is_valid` (QuickReplyShortcutId.h:65-67).
    pub fn is_valid(self) -> bool {
        self.0 > 0
    }

    /// `QuickReplyShortcutId::is_server` (QuickReplyShortcutId.h:69-71).
    pub fn is_server(self) -> bool {
        self.0 > 0 && self.0 <= QuickReplyShortcutId::MAX_SERVER_SHORTCUT_ID
    }

    /// `QuickReplyShortcutId::is_local` (QuickReplyShortcutId.h:73-75).
    pub fn is_local(self) -> bool {
        self.0 > QuickReplyShortcutId::MAX_SERVER_SHORTCUT_ID
    }

    /// `QuickReplyShortcutId::store` (QuickReplyShortcutId.h:77-79).
    pub fn store(self, out: &mut Vec<u8>) {
        store_i32(out, self.0);
    }

    /// `td::parse(QuickReplyShortcutId, Parser)` (QuickReplyShortcutId.h:82-84).
    pub fn parse_from(parser: &mut Parser) -> Result<QuickReplyShortcutId, &'static str> {
        Ok(QuickReplyShortcutId(parser.i32_()?))
    }
}

/// `td::QuickReplyMessageFullId` (QuickReplyMessageFullId.h) — the
/// (shortcut, message) pair key.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct QuickReplyMessageFullId {
    quick_reply_shortcut_id: QuickReplyShortcutId,
    message_id: MessageId,
}

impl QuickReplyMessageFullId {
    pub fn new(
        quick_reply_shortcut_id: QuickReplyShortcutId,
        message_id: MessageId,
    ) -> QuickReplyMessageFullId {
        QuickReplyMessageFullId {
            quick_reply_shortcut_id,
            message_id,
        }
    }

    pub fn get_quick_reply_shortcut_id(self) -> QuickReplyShortcutId {
        self.quick_reply_shortcut_id
    }

    pub fn get_message_id(self) -> MessageId {
        self.message_id
    }

    /// `QuickReplyMessageFullId::is_valid` (QuickReplyMessageFullId.h:47-49).
    pub fn is_valid(self) -> bool {
        self.quick_reply_shortcut_id.is_valid() && self.message_id.is_valid()
    }

    /// `QuickReplyMessageFullId::is_server` (QuickReplyMessageFullId.h:51-53).
    pub fn is_server(self) -> bool {
        self.quick_reply_shortcut_id.is_valid() && self.message_id.is_server()
    }

    /// `QuickReplyMessageFullId::store` (QuickReplyMessageFullId.h:55-59).
    pub fn store(self, out: &mut Vec<u8>) {
        self.quick_reply_shortcut_id.store(out);
        self.message_id.store(out);
    }

    /// `td::parse(QuickReplyMessageFullId, Parser)`
    /// (QuickReplyMessageFullId.h:62-66).
    pub fn parse_from(parser: &mut Parser) -> Result<QuickReplyMessageFullId, &'static str> {
        Ok(QuickReplyMessageFullId::new(
            QuickReplyShortcutId::parse_from(parser)?,
            MessageId::parse_from(parser)?,
        ))
    }
}

/// `td::EphemeralMessageFullId` (EphemeralMessageFullId.h) — the
/// (dialog, ephemeral message) pair key.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EphemeralMessageFullId {
    dialog_id: DialogId,
    ephemeral_message_id: EphemeralMessageId,
}

impl EphemeralMessageFullId {
    pub fn new(
        dialog_id: DialogId,
        ephemeral_message_id: EphemeralMessageId,
    ) -> EphemeralMessageFullId {
        EphemeralMessageFullId {
            dialog_id,
            ephemeral_message_id,
        }
    }

    pub fn get_dialog_id(self) -> DialogId {
        self.dialog_id
    }

    pub fn get_ephemeral_message_id(self) -> EphemeralMessageId {
        self.ephemeral_message_id
    }

    /// `EphemeralMessageFullId::is_valid` (EphemeralMessageFullId.h:38-40).
    pub fn is_valid(self) -> bool {
        self.dialog_id.is_valid() && self.ephemeral_message_id.is_valid()
    }

    /// `EphemeralMessageFullId::store` (EphemeralMessageFullId.h:42-46).
    pub fn store(self, out: &mut Vec<u8>) {
        self.dialog_id.store(out);
        self.ephemeral_message_id.store(out);
    }

    /// `td::parse(EphemeralMessageFullId, Parser)` (EphemeralMessageFullId.h:49-53).
    pub fn parse_from(parser: &mut Parser) -> Result<EphemeralMessageFullId, &'static str> {
        Ok(EphemeralMessageFullId::new(
            DialogId::parse_from(parser)?,
            EphemeralMessageId(parser.i32_()?),
        ))
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
        store_i32(out, self.0);
    }

    pub fn parse_value(input: &[u8]) -> Result<NotificationGroupId, &'static str> {
        let mut parser = Parser::new(input);
        let id = parser.i32_()?;
        parser.fetch_end()?;
        Ok(NotificationGroupId(id))
    }

    /// `td::parse(NotificationGroupId, Parser)` — the parser form the
    /// database layers use.
    pub fn parse_from(parser: &mut Parser) -> Result<NotificationGroupId, &'static str> {
        Ok(NotificationGroupId::new(parser.i32_()?))
    }
}

impl std::fmt::Display for NotificationGroupId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "notification group {}", self.0)
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
        store_i32(out, self.0);
    }

    pub fn parse_value(input: &[u8]) -> Result<NotificationId, &'static str> {
        let mut parser = Parser::new(input);
        let id = parser.i32_()?;
        parser.fetch_end()?;
        Ok(NotificationId(id))
    }

    /// `td::parse(NotificationId, Parser)` — the parser form the database
    /// layers use.
    pub fn parse_from(parser: &mut Parser) -> Result<NotificationId, &'static str> {
        Ok(NotificationId::new(parser.i32_()?))
    }
}

impl std::fmt::Display for NotificationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "notification {}", self.0)
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
        NotificationObjectId(MessageId::max().get())
    }

    pub fn is_valid(self) -> bool {
        self.0 > 0
    }

    pub fn get(self) -> i64 {
        self.0
    }

    pub fn store(self, out: &mut Vec<u8>) {
        store_i64(out, self.0);
    }

    pub fn parse_value(input: &[u8]) -> Result<NotificationObjectId, &'static str> {
        let mut parser = Parser::new(input);
        let id = parser.i64_()?;
        parser.fetch_end()?;
        Ok(NotificationObjectId(id))
    }
}

impl std::fmt::Display for NotificationObjectId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "notification object {}", self.0)
    }
}

/// `td::NotificationGroupKey` (NotificationGroupKey.h) — the sort key for the
/// group list; the C++ `operator<` orders by *descending* notification date,
/// then descending dialog id, then descending group id (most recent first).
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
            log_error!("Failed to reuse changed {}", self.group_id);
            return NotificationGroupId::default();
        }
        self.try_reuse = false;
        if !self.is_valid() {
            log_error!("Failed to reuse invalid {}", self.group_id);
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
        store_i32(out, self.last_notification_date);
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

/// `td::InputDialogId` (InputDialogId.{h,cpp}) — a dialog id paired with the
/// access hash needed to address it from the client.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct InputDialogId {
    dialog_id: DialogId,
    access_hash: i64,
}

impl InputDialogId {
    pub fn new(dialog_id: DialogId) -> InputDialogId {
        InputDialogId {
            dialog_id,
            access_hash: 0,
        }
    }

    pub fn with_access_hash(dialog_id: DialogId, access_hash: i64) -> InputDialogId {
        InputDialogId {
            dialog_id,
            access_hash,
        }
    }

    /// `InputDialogId(const tl_object_ptr<telegram_api::InputPeer> &)`
    /// (InputDialogId.cpp:18-54); unsupported/invalid peers log and decode to
    /// the default value, like the C++ fallthrough.
    pub fn from_input_peer(input_peer: &telegram_api::InputPeer) -> InputDialogId {
        match input_peer {
            telegram_api::InputPeer::inputPeerUser(input_user) => {
                let user_id = UserId::new(input_user.user_id);
                if user_id.is_valid() {
                    return InputDialogId::with_access_hash(
                        DialogId::from_user_id(user_id),
                        input_user.access_hash,
                    );
                }
            }
            telegram_api::InputPeer::inputPeerChat(input_chat) => {
                let chat_id = ChatId::new(input_chat.chat_id);
                if chat_id.is_valid() {
                    return InputDialogId::new(DialogId::from_chat_id(chat_id));
                }
            }
            telegram_api::InputPeer::inputPeerChannel(input_channel) => {
                let channel_id = ChannelId::new(input_channel.channel_id);
                if channel_id.is_valid() {
                    return InputDialogId::with_access_hash(
                        DialogId::from_channel_id(channel_id),
                        input_channel.access_hash,
                    );
                }
            }
            _ => (),
        }
        log_error!("Receive invalid input peer");
        InputDialogId::default()
    }

    /// `InputDialogId::get_input_dialog_ids` (InputDialogId.cpp:56-72); the
    /// C++ optional dedup set becomes an owned `Vec` out-parameter.
    pub fn get_input_dialog_ids(
        input_peers: &[telegram_api::InputPeer],
        added_dialog_ids: &mut Vec<DialogId>,
    ) -> Vec<InputDialogId> {
        let mut result = Vec::with_capacity(input_peers.len());
        for input_peer in input_peers {
            let input_dialog_id = InputDialogId::from_input_peer(input_peer);
            let dialog_id = input_dialog_id.get_dialog_id();
            if input_dialog_id.is_valid() && !added_dialog_ids.contains(&dialog_id) {
                added_dialog_ids.push(dialog_id);
                result.push(input_dialog_id);
            }
        }
        result
    }

    /// `InputDialogId::get_dialog_ids` (InputDialogId.cpp:74-76).
    pub fn get_dialog_ids(input_dialog_ids: &[InputDialogId]) -> Vec<DialogId> {
        input_dialog_ids
            .iter()
            .map(|id| id.get_dialog_id())
            .collect()
    }

    /// `InputDialogId::get_input_dialog_peers` (InputDialogId.cpp:78-89) —
    /// peers that fail to convert are silently skipped, as in the C++.
    pub fn get_input_dialog_peers(
        input_dialog_ids: &[InputDialogId],
    ) -> Vec<telegram_api::InputDialogPeer> {
        let mut result = Vec::with_capacity(input_dialog_ids.len());
        for input_dialog_id in input_dialog_ids {
            if let Some(input_peer) = input_dialog_id.get_input_peer() {
                result.push(telegram_api::InputDialogPeer::inputDialogPeer(
                    telegram_api::inputDialogPeer {
                        peer: Some(Box::new(input_peer)),
                    },
                ));
            }
        }
        result
    }

    /// `InputDialogId::get_input_peers` (InputDialogId.cpp:91-101).
    pub fn get_input_peers(input_dialog_ids: &[InputDialogId]) -> Vec<telegram_api::InputPeer> {
        let mut result = Vec::with_capacity(input_dialog_ids.len());
        for input_dialog_id in input_dialog_ids {
            let input_peer = input_dialog_id.get_input_peer();
            assert!(input_peer.is_some());
            result.push(input_peer.unwrap());
        }
        result
    }

    /// `InputDialogId::get_input_peer` (InputDialogId.cpp:103-118); `None` is
    /// the C++ nullptr for secret chats and invalid dialogs.
    pub fn get_input_peer(self) -> Option<telegram_api::InputPeer> {
        match self.dialog_id.get_type() {
            DialogType::User => Some(telegram_api::InputPeer::inputPeerUser(
                telegram_api::inputPeerUser {
                    user_id: self.dialog_id.get_user_id().get(),
                    access_hash: self.access_hash,
                },
            )),
            DialogType::Chat => Some(telegram_api::InputPeer::inputPeerChat(
                telegram_api::inputPeerChat {
                    chat_id: self.dialog_id.get_chat_id().get(),
                },
            )),
            DialogType::Channel => Some(telegram_api::InputPeer::inputPeerChannel(
                telegram_api::inputPeerChannel {
                    channel_id: self.dialog_id.get_channel_id().get(),
                    access_hash: self.access_hash,
                },
            )),
            DialogType::SecretChat | DialogType::None => None,
        }
    }

    /// `InputDialogId::are_equivalent` (InputDialogId.cpp:120-140) — equality
    /// ignoring secret-chat entries.
    pub fn are_equivalent(lhs: &[InputDialogId], rhs: &[InputDialogId]) -> bool {
        let mut lhs_it = 0usize;
        let mut rhs_it = 0usize;
        while lhs_it < lhs.len() || rhs_it < rhs.len() {
            while lhs_it < lhs.len()
                && lhs[lhs_it].get_dialog_id().get_type() == DialogType::SecretChat
            {
                lhs_it += 1;
            }
            while rhs_it < rhs.len()
                && rhs[rhs_it].get_dialog_id().get_type() == DialogType::SecretChat
            {
                rhs_it += 1;
            }
            if lhs_it >= lhs.len() || rhs_it >= rhs.len() {
                break;
            }
            if lhs[lhs_it].get_dialog_id() != rhs[rhs_it].get_dialog_id() {
                return false;
            }
            lhs_it += 1;
            rhs_it += 1;
        }
        lhs_it >= lhs.len() && rhs_it >= rhs.len()
    }

    /// `InputDialogId::contains` (InputDialogId.cpp:142-149).
    pub fn contains(input_dialog_ids: &[InputDialogId], dialog_id: DialogId) -> bool {
        input_dialog_ids
            .iter()
            .any(|id| id.get_dialog_id() == dialog_id)
    }

    /// `InputDialogId::remove` (InputDialogId.cpp:151-155) — returns whether
    /// anything was removed, like `td::remove_if`.
    pub fn remove(input_dialog_ids: &mut Vec<InputDialogId>, dialog_id: DialogId) -> bool {
        let old_len = input_dialog_ids.len();
        input_dialog_ids.retain(|id| id.get_dialog_id() != dialog_id);
        input_dialog_ids.len() != old_len
    }

    pub fn is_valid(self) -> bool {
        self.dialog_id.is_valid()
    }

    pub fn get_dialog_id(self) -> DialogId {
        self.dialog_id
    }

    /// `InputDialogId::store` (InputDialogId.h:65-70).
    pub fn store(self, out: &mut Vec<u8>) {
        self.dialog_id.store(out);
        store_i64(out, self.access_hash);
    }

    /// `InputDialogId::parse` (InputDialogId.h:72-75).
    pub fn parse_value(input: &[u8]) -> Result<InputDialogId, &'static str> {
        let mut parser = Parser::new(input);
        let dialog_id = parser.i64_()?;
        let access_hash = parser.i64_()?;
        parser.fetch_end()?;
        Ok(InputDialogId {
            dialog_id: DialogId(dialog_id),
            access_hash,
        })
    }
}

impl std::fmt::Display for InputDialogId {
    /// `operator<<(StringBuilder, InputDialogId)` (InputDialogId.h:78-80).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "input {}", self.dialog_id)
    }
}

/// `td::MessageSelfDestructType` (MessageSelfDestructType.{h,cpp}) — the ttl
/// encoding: `0` empty, `0x7FFFFFFF` immediate, `0x7FFFFFFE` the forced
/// replacement for non-allow_immediate immediate requests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct MessageSelfDestructType {
    ttl: i32,
}

impl MessageSelfDestructType {
    /// `MessageSelfDestructType(int32 ttl, bool allow_immediate)`
    /// (MessageSelfDestructType.h:31-36).
    pub fn with_ttl(ttl: i32, allow_immediate: bool) -> MessageSelfDestructType {
        let mut result = MessageSelfDestructType { ttl };
        if !allow_immediate && result.is_immediate() {
            result.ttl = 0x7FFFFFFE;
        }
        result
    }

    pub fn is_valid(self) -> bool {
        self.ttl > 0
    }

    pub fn is_empty(self) -> bool {
        self.ttl == 0
    }

    pub fn is_immediate(self) -> bool {
        self.ttl == 0x7FFFFFFF
    }

    /// `MessageSelfDestructType::is_secret_message_content`
    /// (MessageSelfDestructType.cpp:23-28).
    pub fn is_secret_message_content(self, content_type: MessageContentType) -> bool {
        if self.ttl <= 0 || self.ttl > 60 {
            return self.is_immediate();
        }
        can_be_secret_message_content(content_type)
    }

    /// `MessageSelfDestructType::ensure_at_least` (MessageSelfDestructType.cpp:30-35).
    pub fn ensure_at_least(&mut self, ttl: i32) {
        if self.is_immediate() || self.ttl >= ttl || self.ttl <= 0 || ttl <= 0 {
            return;
        }
        self.ttl = ttl;
    }

    /// `MessageSelfDestructType::get_message_self_destruct_type`
    /// (MessageSelfDestructType.cpp:37-59).
    pub fn get_message_self_destruct_type(
        self_destruct_type: Option<td_api::MessageSelfDestructType>,
    ) -> Result<MessageSelfDestructType, Status> {
        let self_destruct_type = match self_destruct_type {
            Some(ty) => ty,
            None => return Ok(MessageSelfDestructType::default()),
        };
        match self_destruct_type {
            td_api::MessageSelfDestructType::messageSelfDestructTypeTimer(timer) => {
                let ttl = timer.self_destruct_time;

                const MAX_PRIVATE_MESSAGE_TTL: i32 = 60; // server-side limit
                if ttl <= 0 || ttl > MAX_PRIVATE_MESSAGE_TTL {
                    return Err(Status::error(
                        400,
                        "Invalid message content self-destruct time specified",
                    ));
                }
                Ok(MessageSelfDestructType::with_ttl(ttl, true))
            }
            td_api::MessageSelfDestructType::messageSelfDestructTypeImmediately(_) => {
                Ok(MessageSelfDestructType::with_ttl(0x7FFFFFFF, true))
            }
        }
    }

    /// `MessageSelfDestructType::get_message_self_destruct_type_object`
    /// (MessageSelfDestructType.cpp:61-70); `None` is the C++ nullptr.
    pub fn get_message_self_destruct_type_object(self) -> Option<td_api::MessageSelfDestructType> {
        if self.is_immediate() {
            return Some(
                td_api::MessageSelfDestructType::messageSelfDestructTypeImmediately(
                    td_api::messageSelfDestructTypeImmediately,
                ),
            );
        }
        if !self.is_empty() {
            return Some(
                td_api::MessageSelfDestructType::messageSelfDestructTypeTimer(
                    td_api::messageSelfDestructTypeTimer {
                        self_destruct_time: self.ttl,
                    },
                ),
            );
        }
        None
    }

    /// `MessageSelfDestructType::get_input_ttl` (MessageSelfDestructType.cpp:72-74).
    pub fn get_input_ttl(self) -> i32 {
        self.ttl
    }

    pub fn store(self, out: &mut Vec<u8>) {
        store_i32(out, self.ttl);
    }

    pub fn parse_value(input: &[u8]) -> Result<MessageSelfDestructType, &'static str> {
        let mut parser = Parser::new(input);
        let ttl = parser.i32_()?;
        parser.fetch_end()?;
        Ok(MessageSelfDestructType { ttl })
    }
}

impl std::fmt::Display for MessageSelfDestructType {
    /// `operator<<(StringBuilder, MessageSelfDestructType)`
    /// (MessageSelfDestructType.cpp:80-88).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_empty() {
            return write!(f, "non-self-destruct");
        }
        if self.is_immediate() {
            return write!(f, "self-destruct immediately");
        }
        write!(f, "self-destruct at {}", self.get_input_ttl())
    }
}

/// `td::MessageTtl` (MessageTtl.{h,cpp}) — the dialog message-auto-delete
/// period; negative periods log and clamp to 0 like the C++ constructor.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct MessageTtl {
    period: i32,
}

impl MessageTtl {
    /// `MessageTtl(int32 period, const char *source)` (MessageTtl.cpp:12-17).
    pub fn new(period: i32) -> MessageTtl {
        let mut result = MessageTtl { period };
        if result.period < 0 {
            log_error!("Receive message auto-delete time {}", result.period);
            result.period = 0;
        }
        result
    }

    pub fn is_empty(self) -> bool {
        self.period == 0
    }

    /// `MessageTtl::get_message_auto_delete_time_object` (MessageTtl.cpp:19-21).
    pub fn get_message_auto_delete_time_object(self) -> i32 {
        self.period.max(0)
    }

    /// `MessageTtl::get_input_ttl_period` (MessageTtl.cpp:23-25).
    pub fn get_input_ttl_period(self) -> i32 {
        self.period
    }

    pub fn store(self, out: &mut Vec<u8>) {
        store_i32(out, self.period);
    }

    pub fn parse_value(input: &[u8]) -> Result<MessageTtl, &'static str> {
        let mut parser = Parser::new(input);
        let period = parser.i32_()?;
        parser.fetch_end()?;
        Ok(MessageTtl { period })
    }
}

impl std::fmt::Display for MessageTtl {
    /// `operator<<(StringBuilder, MessageTtl)` (MessageTtl.cpp:33-35).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MessageTtl[{}]", self.period)
    }
}

/// `td::MessageEffectId` (MessageEffectId.h).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct MessageEffectId(i64);

impl MessageEffectId {
    pub fn new(id: i64) -> MessageEffectId {
        MessageEffectId(id)
    }

    pub fn is_valid(self) -> bool {
        self.0 != 0
    }

    pub fn get(self) -> i64 {
        self.0
    }

    pub fn store(self, out: &mut Vec<u8>) {
        store_i64(out, self.0);
    }

    pub fn parse_value(input: &[u8]) -> Result<MessageEffectId, &'static str> {
        let mut parser = Parser::new(input);
        let id = parser.i64_()?;
        parser.fetch_end()?;
        Ok(MessageEffectId(id))
    }

    /// The `ParserT` overload (MessageEffectId.hpp) for the nested
    /// store/parse planes.
    pub fn parse_from(parser: &mut Parser) -> Result<MessageEffectId, &'static str> {
        Ok(MessageEffectId(parser.i64_()?))
    }
}

impl std::fmt::Display for MessageEffectId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "message effect {}", self.0)
    }
}

/// `td::SavedMessagesTopicId` (SavedMessagesTopicId.h) — the wrapper over the
/// topic's dialog id. The forward-info constructor and the `Td *`-dependent
/// accessors (input peer/user, topic type object, dependencies) are deferred
/// to the manager tasks that have the manager context.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct SavedMessagesTopicId {
    dialog_id: DialogId,
}

impl SavedMessagesTopicId {
    pub fn new(dialog_id: DialogId) -> SavedMessagesTopicId {
        SavedMessagesTopicId { dialog_id }
    }

    pub fn is_valid(self) -> bool {
        self.dialog_id.is_valid()
    }

    pub fn get_dialog_id(self) -> DialogId {
        self.dialog_id
    }

    /// `SavedMessagesTopicId::get_unique_id` (SavedMessagesTopicId.h:48-50).
    pub fn get_unique_id(self) -> i64 {
        self.dialog_id.get()
    }

    /// `SavedMessagesTopicId::store` (SavedMessagesTopicId.h:76-78).
    pub fn store(self, out: &mut Vec<u8>) {
        self.dialog_id.store(out);
    }

    /// `SavedMessagesTopicId::parse` (SavedMessagesTopicId.h:80-82).
    pub fn parse_from(parser: &mut Parser) -> Result<SavedMessagesTopicId, &'static str> {
        Ok(SavedMessagesTopicId::new(DialogId::parse_from(parser)?))
    }

    pub fn parse_value(input: &[u8]) -> Result<SavedMessagesTopicId, &'static str> {
        let dialog_id = DialogId::parse_value(input)?;
        Ok(SavedMessagesTopicId { dialog_id })
    }
}

/// `MessagesManager::NotificationInfo` (MessagesManager.h:1290-1301) — the
/// per-dialog notification group bookkeeping; created lazily in the C++
/// (`unique_ptr`), so the manager holds it in an `Option`.
#[derive(Clone, Debug, Default)]
pub struct NotificationInfo {
    pub message_notification_group: NotificationGroupInfo,
    pub mention_notification_group: NotificationGroupInfo,
    /// secret chats only
    pub new_secret_chat_notification_id: NotificationId,
    pub pinned_message_notification_message_id: MessageId,
    pub max_push_notification_message_id: MessageId,

    pub pending_new_message_notifications: Vec<(DialogId, MessageId)>,
    pub pending_new_mention_notifications: Vec<(DialogId, MessageId)>,

    pub notification_id_to_message_id: std::collections::HashMap<NotificationId, MessageId>,
}

/// `MessagesManager::EphemeralMessageInfo` (MessagesManager.h:1303-1306).
#[derive(Clone, Copy, Debug, Default)]
pub struct EphemeralMessageInfo {
    pub anchor_message_id: MessageId,
    pub monotonic_id: u32,
}

// ---------------------------------------------------------------------------
// M.10.1b — the companion value plane of the Message/Dialog structs
// (StoryId.{h}, StoryFullId.h, DialogFilterId.h, FolderId.h, DialogListId.h,
// MessageOrigin.h, FactCheck.h, ReactionType.h, ChatReactions.h, MinChannel.h,
// MessageReplyInfo.h, NotificationSoundType.h, NotificationSound.h,
// DialogNotificationSettings.h, DialogActionBar.h, BusinessBotManageBar.h).
//
// The inline C++ store/parse bodies are ported byte-exactly; formats that
// live in the .cpp files land with the slices that consume them (the
// wire/td_api conversions likewise). Documented stand-ins:
// - `SuggestedPost`/`SuggestedPostPrice` are NOT defined here — batch 2
//   re-points `SuggestedPost.price` at the M.9-line `message_content.rs`
//   `SuggestedPostPrice` (which arrives with the M.9.1 merge); a local copy
//   here would be a duplicate definition the merge would have to reconcile.
// - `MinChannel` drops `DialogPhoto`/`AccentColorId` (they live on the
//   unmerged m54 line; the fields arrive at that integration).
// - `MessageReactions` is an opaque placeholder until the Wave-2
//   ReactionManager slice.
// ---------------------------------------------------------------------------

/// `td::StoryId` (StoryId.h) — a per-dialog story identifier.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct StoryId(i32);

impl StoryId {
    /// `StoryId::MAX_SERVER_STORY_ID` (StoryId.h:22).
    pub const MAX_SERVER_STORY_ID: i32 = 1_999_999_999;

    pub fn new(story_id: i32) -> StoryId {
        StoryId(story_id)
    }

    /// `StoryId::get_input_story_ids` (StoryId.h:31-38).
    pub fn get_input_story_ids(story_ids: &[StoryId]) -> Vec<i32> {
        story_ids.iter().map(|story_id| story_id.get()).collect()
    }

    /// `StoryId::get_story_ids` (StoryId.h:40-47).
    pub fn get_story_ids(input_story_ids: &[i32]) -> Vec<StoryId> {
        input_story_ids
            .iter()
            .map(|&input_story_id| StoryId(input_story_id))
            .collect()
    }

    pub fn get(self) -> i32 {
        self.0
    }

    /// `StoryId::is_valid` (StoryId.h:61-63).
    pub fn is_valid(self) -> bool {
        self.0 > 0
    }

    /// `StoryId::is_server` (StoryId.h:65-67).
    pub fn is_server(self) -> bool {
        self.0 > 0 && self.0 <= Self::MAX_SERVER_STORY_ID
    }

    /// `StoryId::store` (StoryId.h:69-72).
    pub fn store(self, out: &mut Vec<u8>) {
        store_i32(out, self.0);
    }

    /// `StoryId::parse` (StoryId.h:74-77).
    pub fn parse_value(input: &[u8]) -> Result<StoryId, &'static str> {
        let mut parser = Parser::new(input);
        let id = parser.i32_()?;
        parser.fetch_end()?;
        Ok(StoryId(id))
    }
}

impl std::fmt::Display for StoryId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "story {}", self.0)
    }
}

/// `td::StoryFullId` (StoryFullId.h) — a story together with its owner dialog.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StoryFullId {
    pub dialog_id: DialogId,
    pub story_id: StoryId,
}

impl StoryFullId {
    pub fn new(dialog_id: DialogId, story_id: StoryId) -> StoryFullId {
        StoryFullId {
            dialog_id,
            story_id,
        }
    }

    pub fn get_dialog_id(self) -> DialogId {
        self.dialog_id
    }

    pub fn get_story_id(self) -> StoryId {
        self.story_id
    }

    /// `StoryFullId::is_valid` (StoryFullId.h:46-48).
    pub fn is_valid(self) -> bool {
        self.dialog_id.is_valid() && self.story_id.is_valid()
    }

    /// `StoryFullId::is_server` (StoryFullId.h:50-52).
    pub fn is_server(self) -> bool {
        self.dialog_id.is_valid() && self.story_id.is_server()
    }

    /// `StoryFullId::store` (StoryFullId.h:54-58).
    pub fn store(self, out: &mut Vec<u8>) {
        self.dialog_id.store(out);
        self.story_id.store(out);
    }

    /// `StoryFullId::parse` (StoryFullId.h:60-64).
    pub fn parse_value(input: &[u8]) -> Result<StoryFullId, &'static str> {
        let mut parser = Parser::new(input);
        let dialog_id = parser.i64_()?;
        let story_id = parser.i32_()?;
        parser.fetch_end()?;
        Ok(StoryFullId::new(DialogId(dialog_id), StoryId(story_id)))
    }

    /// The `ParserT` overload of `StoryFullId::parse` (StoryFullId.hpp) —
    /// used from the nested store/parse planes (the ids are stored through
    /// `DialogId::store`/`StoryId::store`, so parse through those too).
    pub fn parse_from(parser: &mut Parser) -> Result<StoryFullId, &'static str> {
        let dialog_id = DialogId::parse_from(parser)?;
        let story_id = StoryId(parser.i32_()?);
        Ok(StoryFullId {
            dialog_id,
            story_id,
        })
    }
}

/// `td::StoryFullIdHash` (StoryFullId.h:67-71) — `combine_hashes` over the
/// C++ `DialogIdHash`/`StoryIdHash`, not a derived hash.
impl std::hash::Hash for StoryFullId {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        state.write_u32(combine_hashes(
            hash_int64(self.dialog_id.get()),
            hash_uint32(self.story_id.get() as u32),
        ));
    }
}

impl std::fmt::Display for StoryFullId {
    /// `operator<<(StringBuilder&, StoryFullId)` (StoryFullId.h:73-75).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} in {}", self.story_id, self.dialog_id)
    }
}

/// `td::DialogFilterId` (DialogFilterId.h) — a chat-folder filter identifier.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DialogFilterId(i32);

impl DialogFilterId {
    /// `DialogFilterId::min` (DialogFilterId.h:30-32).
    pub const MIN_FILTER_ID: i32 = 2;
    /// `DialogFilterId::max` (DialogFilterId.h:33-35).
    pub const MAX_FILTER_ID: i32 = 255;

    pub const fn new(dialog_filter_id: i32) -> DialogFilterId {
        DialogFilterId(dialog_filter_id)
    }

    /// `DialogFilterId::get_dialog_filter_ids` (DialogFilterId.h:37-44).
    pub fn get_dialog_filter_ids(chat_folder_ids: &[i32]) -> Vec<DialogFilterId> {
        chat_folder_ids
            .iter()
            .map(|&chat_folder_id| DialogFilterId(chat_folder_id))
            .collect()
    }

    /// `DialogFilterId::is_valid` (DialogFilterId.h:46-49) — the `max()` bound
    /// is deliberately not checked.
    pub fn is_valid(self) -> bool {
        self.0 >= Self::MIN_FILTER_ID
    }

    pub fn get(self) -> i32 {
        self.0
    }

    /// `DialogFilterId::store` (DialogFilterId.h:67-70).
    pub fn store(self, out: &mut Vec<u8>) {
        store_i32(out, self.0);
    }

    /// `DialogFilterId::parse` (DialogFilterId.h:72-75).
    pub fn parse_value(input: &[u8]) -> Result<DialogFilterId, &'static str> {
        let mut parser = Parser::new(input);
        let id = parser.i32_()?;
        parser.fetch_end()?;
        Ok(DialogFilterId(id))
    }
}

impl std::fmt::Display for DialogFilterId {
    /// (DialogFilterId.h:84-86) — C++ prints "folder", not "filter".
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "folder {}", self.0)
    }
}

/// `td::FolderId` (FolderId.h) — the archive/main dialog folder; the only
/// valid non-main folder is 1.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FolderId(i32);

impl FolderId {
    pub fn new(folder_id: i32) -> FolderId {
        FolderId(if folder_id != 1 { 0 } else { folder_id })
    }

    /// `FolderId::main` (FolderId.h:56-58).
    pub fn main() -> FolderId {
        FolderId(0)
    }

    /// `FolderId::archive` (FolderId.h:59-61).
    pub fn archive() -> FolderId {
        FolderId(1)
    }

    pub fn get(self) -> i32 {
        self.0
    }

    /// `FolderId::store` (FolderId.h:43-46).
    pub fn store(self, out: &mut Vec<u8>) {
        store_i32(out, self.0);
    }

    /// `FolderId::parse` (FolderId.h:48-54) — the clamp is re-applied on read.
    pub fn parse_value(input: &[u8]) -> Result<FolderId, &'static str> {
        let mut parser = Parser::new(input);
        let id = parser.i32_()?;
        parser.fetch_end()?;
        Ok(FolderId::new(id))
    }

    /// The archive/main folder constants, the Rust-idiomatic form of the
    /// `main()`/`archive()` constructors.
    pub const MAIN: FolderId = FolderId(0);
    pub const ARCHIVE: FolderId = FolderId(1);
}

/// `td::MessageSearchFilter` (MessageSearchFilter.h:18-41) — append only
/// before `Size`; the discriminants index the per-filter database
/// statements and the `message_index_<i>` partial-index mask space.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(i32)]
pub enum MessageSearchFilter {
    Empty = 0,
    Animation,
    Audio,
    Document,
    Photo,
    Video,
    VoiceNote,
    PhotoAndVideo,
    Url,
    ChatPhoto,
    Call,
    MissedCall,
    VideoNote,
    VoiceAndVideoNote,
    Mention,
    UnreadMention,
    FailedToSend,
    Pinned,
    UnreadReaction,
    Poll,
    UnreadPollVote,
}

impl MessageSearchFilter {
    /// `MessageSearchFilter::Size` — one past the last real filter.
    pub const SIZE: i32 = 21;

    pub fn as_i32(self) -> i32 {
        self as i32
    }
}

/// The C++ struct members default to `MessageSearchFilter::Empty`.
impl Default for MessageSearchFilter {
    fn default() -> MessageSearchFilter {
        MessageSearchFilter::Empty
    }
}

/// `message_search_filter_count` (MessageSearchFilter.h:43-45).
pub fn message_search_filter_count() -> usize {
    (MessageSearchFilter::SIZE - 1) as usize
}

/// `message_search_filter_index` (MessageSearchFilter.h:47-50) — `CHECK`s
/// non-empty, returns the zero-based statement slot.
pub fn message_search_filter_index(filter: MessageSearchFilter) -> i32 {
    assert!(filter != MessageSearchFilter::Empty);
    filter.as_i32() - 1
}

/// `message_search_filter_index_mask` (MessageSearchFilter.h:52-57).
pub fn message_search_filter_index_mask(filter: MessageSearchFilter) -> i32 {
    if filter == MessageSearchFilter::Empty {
        return 0;
    }
    1 << message_search_filter_index(filter)
}

/// `call_message_search_filter_index` (MessageSearchFilter.h:59-62) — the
/// `full_message_index_<i>` slot (0 for Call, 1 for MissedCall).
pub fn call_message_search_filter_index(filter: MessageSearchFilter) -> i32 {
    assert!(filter == MessageSearchFilter::Call || filter == MessageSearchFilter::MissedCall);
    filter.as_i32() - MessageSearchFilter::Call.as_i32()
}

impl std::fmt::Display for FolderId {
    /// (FolderId.h:70-72).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "folder {}", self.0)
    }
}

/// `td::DialogListId` (DialogListId.h) — a FolderId or a DialogFilterId
/// packed into one i64; filters live above the `FILTER_ID_SHIFT` offset.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DialogListId(i64);

impl DialogListId {
    /// `DialogListId::FILTER_ID_SHIFT` (DialogListId.h:25).
    pub const FILTER_ID_SHIFT: i64 = 1i64 << 32;

    /// `DialogListId(DialogFilterId)` (DialogListId.h:35-36).
    pub fn from_filter(dialog_filter_id: DialogFilterId) -> DialogListId {
        DialogListId(dialog_filter_id.get() as i64 + Self::FILTER_ID_SHIFT)
    }

    /// `DialogListId(FolderId)` (DialogListId.h:38-39).
    pub fn from_folder(folder_id: FolderId) -> DialogListId {
        DialogListId(folder_id.get() as i64)
    }

    pub fn get(self) -> i64 {
        self.0
    }

    /// `DialogListId::is_folder` (DialogListId.h:60-62).
    pub fn is_folder(self) -> bool {
        (i32::MIN as i64) <= self.0 && self.0 <= (i32::MAX as i64)
    }

    /// `DialogListId::is_filter` (DialogListId.h:64-67).
    pub fn is_filter(self) -> bool {
        (i32::MIN as i64 + Self::FILTER_ID_SHIFT) <= self.0
            && self.0 <= (i32::MAX as i64 + Self::FILTER_ID_SHIFT)
    }

    /// `DialogListId::get_folder_id` (DialogListId.h:69-72).
    pub fn get_folder_id(self) -> FolderId {
        assert!(self.is_folder());
        FolderId::new(self.0 as i32)
    }

    /// `DialogListId::get_filter_id` (DialogListId.h:74-77).
    pub fn get_filter_id(self) -> DialogFilterId {
        assert!(self.is_filter());
        DialogFilterId((self.0 - Self::FILTER_ID_SHIFT) as i32)
    }
}

/// `td::DialogListIdHash` (DialogListId.h:80-84).
impl std::hash::Hash for DialogListId {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        state.write_u32(hash_int64(self.0));
    }
}

/// `td::MessageOrigin` (MessageOrigin.h) — where a forwarded/replied message
/// originally comes from; the wire/td_api conversions live with the
/// receive-path slices.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MessageOrigin {
    pub sender_user_id: UserId,
    pub sender_dialog_id: DialogId,
    pub message_id: MessageId,
    pub author_signature: String,
    pub sender_name: String,
}

impl MessageOrigin {
    pub fn new(
        sender_user_id: UserId,
        sender_dialog_id: DialogId,
        message_id: MessageId,
        author_signature: String,
        sender_name: String,
    ) -> MessageOrigin {
        MessageOrigin {
            sender_user_id,
            sender_dialog_id,
            message_id,
            author_signature,
            sender_name,
        }
    }

    /// `MessageOrigin::is_empty` (MessageOrigin.h:49-52).
    pub fn is_empty(&self) -> bool {
        !self.sender_user_id.is_valid()
            && !self.sender_dialog_id.is_valid()
            && !self.message_id.is_valid()
            && self.author_signature.is_empty()
            && self.sender_name.is_empty()
    }

    /// `MessageOrigin::is_channel_post` (MessageOrigin.h:64-66).
    pub fn is_channel_post(&self) -> bool {
        self.message_id.is_valid()
    }

    /// `MessageOrigin::has_sender_signature` (MessageOrigin.h:68-70).
    pub fn has_sender_signature(&self) -> bool {
        !self.author_signature.is_empty() || !self.sender_name.is_empty()
    }

    /// `MessageOrigin::get_sender_name` (MessageOrigin.h:60-62).
    pub fn get_sender_name(&self) -> &str {
        &self.sender_name
    }

    /// `MessageOrigin::store` (MessageOrigin.hpp:17-42).
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_sender_user_id = self.sender_user_id.is_valid();
        let has_sender_dialog_id = self.sender_dialog_id.is_valid();
        let has_message_id = self.message_id.is_valid();
        let has_author_signature = !self.author_signature.is_empty();
        let has_sender_name = !self.sender_name.is_empty();
        store_flags(
            out,
            [
                has_sender_user_id,
                has_sender_dialog_id,
                has_message_id,
                has_author_signature,
                has_sender_name,
            ],
        );
        if has_sender_user_id {
            self.sender_user_id.store(out);
        }
        if has_sender_dialog_id {
            self.sender_dialog_id.store(out);
        }
        if has_message_id {
            self.message_id.store(out);
        }
        if has_author_signature {
            store_string(out, &self.author_signature);
        }
        if has_sender_name {
            store_string(out, &self.sender_name);
        }
    }

    /// `MessageOrigin::parse` (MessageOrigin.hpp:44-71). The C++ `CHECK`
    /// that a stored sender dialog is a channel stays an assert.
    pub fn parse_from(parser: &mut Parser) -> Result<MessageOrigin, &'static str> {
        let [
            has_sender_user_id,
            has_sender_dialog_id,
            has_message_id,
            has_author_signature,
            has_sender_name,
        ] = parser.parse_flags::<5>()?;
        let sender_user_id = if has_sender_user_id {
            UserId::parse_from(parser)?
        } else {
            UserId::default()
        };
        let sender_dialog_id = if has_sender_dialog_id {
            let dialog_id = DialogId::parse_from(parser)?;
            assert!(dialog_id.get_type() == DialogType::Channel);
            dialog_id
        } else {
            DialogId::default()
        };
        let message_id = if has_message_id {
            MessageId::parse_from(parser)?
        } else {
            MessageId::default()
        };
        let author_signature = if has_author_signature {
            parser.string()?
        } else {
            String::new()
        };
        let sender_name = if has_sender_name {
            parser.string()?
        } else {
            String::new()
        };
        Ok(MessageOrigin {
            sender_user_id,
            sender_dialog_id,
            message_id,
            author_signature,
            sender_name,
        })
    }
}

/// `td::FactCheck` (FactCheck.h) — the fact-check block attached to a message.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FactCheck {
    pub country_code: String,
    pub text: FormattedText,
    pub hash: i64,
    pub need_check: bool,
}

impl FactCheck {
    /// `FactCheck::is_empty` (FactCheck.h:41-43).
    pub fn is_empty(&self) -> bool {
        self.hash == 0
    }

    /// `FactCheck::need_check` (FactCheck.h:45-47).
    pub fn need_check(&self) -> bool {
        self.need_check
    }

    /// `FactCheck::store` (FactCheck.hpp:16-36). The C++ `CHECK(!is_empty())`
    /// is the caller's contract.
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_country_code = !self.country_code.is_empty();
        let has_text = !self.text.text.is_empty();
        store_flags(out, [self.need_check, has_country_code, has_text]);
        store_i64(out, self.hash);
        if has_country_code {
            store_string(out, &self.country_code);
        }
        if has_text {
            self.text.store(out);
        }
    }

    /// `FactCheck::parse` (FactCheck.hpp:38-60).
    pub fn parse_from(parser: &mut Parser) -> Result<FactCheck, &'static str> {
        let [need_check, has_country_code, has_text] = parser.parse_flags::<3>()?;
        let hash = parser.i64_()?;
        let country_code = if has_country_code {
            parser.string()?
        } else {
            String::new()
        };
        let text = if has_text {
            FormattedText::parse(parser)?
        } else {
            FormattedText::default()
        };
        let result = FactCheck {
            country_code,
            text,
            hash,
            need_check,
        };
        if result.is_empty() {
            return Err("Load an empty fact check");
        }
        Ok(result)
    }
}

/// `td::ReactionType` (ReactionType.h) — an emoji reaction, a custom emoji
/// reaction (the internal string starts with `'#'`) or the paid reaction
/// (`"$"`). The wire/td_api conversions and the `Hash<string>`-parity hash
/// land with the Wave-2 reactions slice.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ReactionType(String);

impl ReactionType {
    /// `ReactionType::ReactionType(string &&)` (ReactionType.cpp:36-38).
    pub fn new(reaction: impl Into<String>) -> ReactionType {
        ReactionType(reaction.into())
    }

    /// `ReactionType::is_empty` (ReactionType.h:68-70).
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// `ReactionType::get_string` (ReactionType.h:72-74).
    pub fn get_string(&self) -> &str {
        &self.0
    }

    /// `ReactionType::is_custom_reaction` (ReactionType.cpp:171-173).
    pub fn is_custom_reaction(&self) -> bool {
        self.0.starts_with('#')
    }

    /// `ReactionType::is_paid_reaction` (ReactionType.cpp:175-177).
    pub fn is_paid_reaction(&self) -> bool {
        self.0 == "$"
    }
}

impl std::fmt::Display for ReactionType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// `td::ChatReactions` (ChatReactions.h) — the per-dialog available-reactions
/// set.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChatReactions {
    pub reaction_types: Vec<ReactionType>,
    /// implies empty `reaction_types`
    pub allow_all_regular: bool,
    /// implies `allow_all_regular`
    pub allow_all_custom: bool,
    pub reactions_limit: i32,
    pub paid_reactions_available: bool,
}

impl ChatReactions {
    /// `ChatReactions::legacy` (ChatReactions.h:30-34).
    pub fn legacy(reaction_types: Vec<ReactionType>) -> ChatReactions {
        ChatReactions {
            reaction_types,
            ..ChatReactions::default()
        }
    }

    /// `ChatReactions(bool, bool)` (ChatReactions.h:41-43).
    pub fn from_all_flags(allow_all_regular: bool, allow_all_custom: bool) -> ChatReactions {
        ChatReactions {
            allow_all_regular,
            allow_all_custom,
            ..ChatReactions::default()
        }
    }

    /// `ChatReactions::empty` (ChatReactions.h:56-58).
    pub fn empty(&self) -> bool {
        self.reaction_types.is_empty() && !self.allow_all_regular && !self.paid_reactions_available
    }
}

/// `td::MinChannel` (MinChannel.h) — STAND-IN: `DialogPhoto` and
/// `AccentColorId` live on the unmerged m54 line and arrive with that
/// integration; the `photo`/`accent_color_id` fields are deferred with them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MinChannel {
    pub title: String,
    pub is_megagroup: bool,
}

/// `td::MessageReplyInfo` (MessageReplyInfo.h) — the comment-thread reply
/// info attached to channel posts; the update/need_reget logic lands with the
/// receive-path slices.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessageReplyInfo {
    pub reply_count: i32,
    pub pts: i32,
    /// comments only
    pub recent_replier_dialog_ids: Vec<DialogId>,
    /// comments only
    pub replier_min_channels: Vec<(ChannelId, MinChannel)>,
    /// comments only
    pub channel_id: ChannelId,
    pub max_message_id: MessageId,
    pub last_read_inbox_message_id: MessageId,
    pub last_read_outbox_message_id: MessageId,
    pub is_comment: bool,
    pub is_dropped: bool,
}

impl Default for MessageReplyInfo {
    /// The C++ in-class defaults (MessageReplyInfo.h:26-34): the counters
    /// start at -1 ("unknown"), not 0.
    fn default() -> MessageReplyInfo {
        MessageReplyInfo {
            reply_count: -1,
            pts: -1,
            recent_replier_dialog_ids: Vec::new(),
            replier_min_channels: Vec::new(),
            channel_id: ChannelId::default(),
            max_message_id: MessageId::default(),
            last_read_inbox_message_id: MessageId::default(),
            last_read_outbox_message_id: MessageId::default(),
            is_comment: false,
            is_dropped: false,
        }
    }
}

impl MessageReplyInfo {
    /// `MessageReplyInfo::MAX_RECENT_REPLIERS` (MessageReplyInfo.h:37).
    pub const MAX_RECENT_REPLIERS: usize = 3;

    /// `MessageReplyInfo::is_empty` (MessageReplyInfo.h:43-45).
    pub fn is_empty(&self) -> bool {
        self.reply_count < 0
    }

    /// `MessageReplyInfo::was_dropped` (MessageReplyInfo.h:47-49).
    pub fn was_dropped(&self) -> bool {
        self.is_dropped
    }

    /// `MessageReplyInfo::store` (MessageReplyInfo.hpp:16-51). The C++
    /// `CHECK(!is_empty())` is the caller's contract.
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_recent_replier_dialog_ids = !self.recent_replier_dialog_ids.is_empty();
        let has_channel_id = self.channel_id.is_valid();
        let has_max_message_id = self.max_message_id.is_valid();
        let has_last_read_inbox_message_id = self.last_read_inbox_message_id.is_valid();
        let has_last_read_outbox_message_id = self.last_read_outbox_message_id.is_valid();
        let has_replier_min_channels = !self.replier_min_channels.is_empty();
        store_flags(
            out,
            [
                self.is_comment,
                has_recent_replier_dialog_ids,
                has_channel_id,
                has_max_message_id,
                has_last_read_inbox_message_id,
                has_last_read_outbox_message_id,
                has_replier_min_channels,
            ],
        );
        store_i32(out, self.reply_count);
        store_i32(out, self.pts);
        if has_recent_replier_dialog_ids {
            store_dialog_id_vec(out, &self.recent_replier_dialog_ids);
        }
        if has_channel_id {
            self.channel_id.store(out);
        }
        if has_max_message_id {
            self.max_message_id.store(out);
        }
        if has_last_read_inbox_message_id {
            self.last_read_inbox_message_id.store(out);
        }
        if has_last_read_outbox_message_id {
            self.last_read_outbox_message_id.store(out);
        }
        if has_replier_min_channels {
            store_min_channel_vec(out, &self.replier_min_channels);
        }
    }

    /// `MessageReplyInfo::parse` (MessageReplyInfo.hpp:53-94) — including
    /// the channel id 777 "dropped legacy" special case and the recent
    /// repliers clamp.
    pub fn parse_from(parser: &mut Parser) -> Result<MessageReplyInfo, &'static str> {
        let [
            is_comment,
            has_recent_replier_dialog_ids,
            has_channel_id,
            has_max_message_id,
            has_last_read_inbox_message_id,
            has_last_read_outbox_message_id,
            has_replier_min_channels,
        ] = parser.parse_flags::<7>()?;
        let reply_count = parser.i32_()?;
        let pts = parser.i32_()?;
        let recent_replier_dialog_ids = if has_recent_replier_dialog_ids {
            parse_dialog_id_vec(parser)?
        } else {
            Vec::new()
        };
        let channel_id = if has_channel_id {
            ChannelId::parse_from(parser)?
        } else {
            ChannelId::default()
        };
        let max_message_id = if has_max_message_id {
            MessageId::parse_from(parser)?
        } else {
            MessageId::default()
        };
        let last_read_inbox_message_id = if has_last_read_inbox_message_id {
            MessageId::parse_from(parser)?
        } else {
            MessageId::default()
        };
        let last_read_outbox_message_id = if has_last_read_outbox_message_id {
            MessageId::parse_from(parser)?
        } else {
            MessageId::default()
        };
        let replier_min_channels = if has_replier_min_channels {
            parse_min_channel_vec(parser)?
        } else {
            Vec::new()
        };

        let mut result = MessageReplyInfo {
            reply_count,
            pts,
            recent_replier_dialog_ids,
            replier_min_channels,
            channel_id,
            max_message_id,
            last_read_inbox_message_id,
            last_read_outbox_message_id,
            is_comment,
            is_dropped: false,
        };
        if result.channel_id.get() == 777 {
            result = MessageReplyInfo::default();
            result.is_dropped = true;
        }
        result
            .recent_replier_dialog_ids
            .truncate(Self::MAX_RECENT_REPLIERS);
        Ok(result)
    }
}

/// `td::NotificationSoundType` (NotificationSoundType.h:13).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(i32)]
pub enum NotificationSoundType {
    #[default]
    None = 0,
    Local = 1,
    Ringtone = 2,
}

/// `td::NotificationSound` (NotificationSound.h) — the virtual-base class
/// hierarchy (NotificationSound.cpp:14-44: None/Local/Ringtone) flattened
/// into an enum; the store/parse format lands with the notification-settings
/// slice.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum NotificationSound {
    /// `NotificationSoundNone`
    #[default]
    None,
    /// `NotificationSoundLocal`
    Local(String),
    /// `NotificationSoundRingtone` — a ringtone document id
    Ringtone(i64),
}

impl NotificationSound {
    /// The `NotificationSound::get_type` overloads of the three subclasses.
    pub fn get_type(&self) -> NotificationSoundType {
        match self {
            NotificationSound::None => NotificationSoundType::None,
            NotificationSound::Local(_) => NotificationSoundType::Local,
            NotificationSound::Ringtone(_) => NotificationSoundType::Ringtone,
        }
    }
}

/// `td::DialogNotificationSettings` (DialogNotificationSettings.h) — the
/// per-dialog notification settings with the C++ in-class defaults; the
/// wire/td_api conversions land with M.10.13/M.10.14.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DialogNotificationSettings {
    pub mute_until: i32,
    pub sound: Option<NotificationSound>,
    pub story_sound: Option<NotificationSound>,
    pub show_preview: bool,
    pub mute_stories: bool,
    pub hide_story_sender: bool,
    pub silent_send_message: bool,
    pub use_default_mute_until: bool,
    pub use_default_show_preview: bool,
    pub use_default_mute_stories: bool,
    pub use_default_hide_story_sender: bool,
    pub is_synchronized: bool,
    // local settings
    pub use_default_disable_pinned_message_notifications: bool,
    pub disable_pinned_message_notifications: bool,
    pub use_default_disable_mention_notifications: bool,
    pub disable_mention_notifications: bool,
}

impl Default for DialogNotificationSettings {
    /// The C++ in-class defaults (DialogNotificationSettings.h:21-38).
    fn default() -> DialogNotificationSettings {
        DialogNotificationSettings {
            mute_until: 0,
            sound: None,
            story_sound: None,
            show_preview: true,
            mute_stories: false,
            hide_story_sender: false,
            silent_send_message: false,
            use_default_mute_until: true,
            use_default_show_preview: true,
            use_default_mute_stories: true,
            use_default_hide_story_sender: true,
            is_synchronized: false,
            use_default_disable_pinned_message_notifications: true,
            disable_pinned_message_notifications: false,
            use_default_disable_mention_notifications: true,
            disable_mention_notifications: false,
        }
    }
}

/// `td::DialogActionBar` (DialogActionBar.h) — the peer-report/join-request
/// action bar; the wire conversions and the `.cpp` helpers
/// (`create`/`fix`/`is_empty` and friends) land with the dialog-list slices.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DialogActionBar {
    /// distance to the peer; -1 = unknown
    pub distance: i32,
    pub join_request_date: i32,
    pub join_request_dialog_title: String,
    pub registration_month: i32,
    pub phone_country: i32,
    pub last_name_change_date: i32,
    pub last_photo_change_date: i32,
    pub can_report_spam: bool,
    pub can_add_contact: bool,
    pub can_block_user: bool,
    pub can_share_phone_number: bool,
    pub can_report_location: bool,
    pub can_unarchive: bool,
    pub can_invite_members: bool,
    pub is_join_request_broadcast: bool,
}

impl Default for DialogActionBar {
    /// Mirrors the C++ field initializers (DialogActionBar.h:19-34): every
    /// field zeroes out except `distance`, which starts at -1 ("unknown") —
    /// a derived Default would make `store` treat it as a known distance.
    fn default() -> DialogActionBar {
        DialogActionBar {
            distance: -1,
            join_request_date: 0,
            join_request_dialog_title: String::new(),
            registration_month: 0,
            phone_country: 0,
            last_name_change_date: 0,
            last_photo_change_date: 0,
            can_report_spam: false,
            can_add_contact: false,
            can_block_user: false,
            can_share_phone_number: false,
            can_report_location: false,
            can_unarchive: false,
            can_invite_members: false,
            is_join_request_broadcast: false,
        }
    }
}

impl DialogActionBar {
    pub fn can_report_spam(&self) -> bool {
        self.can_report_spam
    }

    pub fn can_unarchive(&self) -> bool {
        self.can_unarchive
    }

    /// `DialogActionBar::store` (DialogActionBar.h:80-122) — 14 flag words
    /// (C++ `BEGIN_STORE_FLAGS` order preserved) plus the conditional fields.
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_distance = self.distance >= 0;
        let has_join_request = !self.join_request_dialog_title.is_empty();
        let has_registration_month = self.registration_month > 0;
        let has_phone_country = self.phone_country > 0;
        let has_last_name_change_date = self.last_name_change_date > 0;
        let has_last_photo_change_date = self.last_photo_change_date > 0;
        store_flags(
            out,
            [
                self.can_report_spam,
                self.can_add_contact,
                self.can_block_user,
                self.can_share_phone_number,
                self.can_report_location,
                self.can_unarchive,
                self.can_invite_members,
                has_distance,
                self.is_join_request_broadcast,
                has_join_request,
                has_registration_month,
                has_phone_country,
                has_last_name_change_date,
                has_last_photo_change_date,
            ],
        );
        if has_distance {
            store_i32(out, self.distance);
        }
        if has_join_request {
            store_string(out, &self.join_request_dialog_title);
            store_i32(out, self.join_request_date);
        }
        if has_registration_month {
            store_i32(out, self.registration_month);
        }
        if has_phone_country {
            store_i32(out, self.phone_country);
        }
        if has_last_name_change_date {
            store_i32(out, self.last_name_change_date);
        }
        if has_last_photo_change_date {
            store_i32(out, self.last_photo_change_date);
        }
    }

    /// `DialogActionBar::parse` (DialogActionBar.h:124-167).
    pub fn parse_value(input: &[u8]) -> Result<DialogActionBar, &'static str> {
        let mut parser = Parser::new(input);
        let mut result = DialogActionBar::default();
        let [
            can_report_spam,
            can_add_contact,
            can_block_user,
            can_share_phone_number,
            can_report_location,
            can_unarchive,
            can_invite_members,
            has_distance,
            is_join_request_broadcast,
            has_join_request,
            has_registration_month,
            has_phone_country,
            has_last_name_change_date,
            has_last_photo_change_date,
        ] = parser.parse_flags::<14>()?;
        result.can_report_spam = can_report_spam;
        result.can_add_contact = can_add_contact;
        result.can_block_user = can_block_user;
        result.can_share_phone_number = can_share_phone_number;
        result.can_report_location = can_report_location;
        result.can_unarchive = can_unarchive;
        result.can_invite_members = can_invite_members;
        result.is_join_request_broadcast = is_join_request_broadcast;
        if has_distance {
            result.distance = parser.i32_()?;
        }
        if has_join_request {
            result.join_request_dialog_title = parser.string()?;
            result.join_request_date = parser.i32_()?;
        }
        if has_registration_month {
            result.registration_month = parser.i32_()?;
        }
        if has_phone_country {
            result.phone_country = parser.i32_()?;
        }
        if has_last_name_change_date {
            result.last_name_change_date = parser.i32_()?;
        }
        if has_last_photo_change_date {
            result.last_photo_change_date = parser.i32_()?;
        }
        parser.fetch_end()?;
        Ok(result)
    }
}

/// `td::BusinessBotManageBar` (BusinessBotManageBar.h) — the business-bot
/// manage bar shown in private dialogs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BusinessBotManageBar {
    pub business_bot_user_id: UserId,
    pub business_bot_manage_url: String,
    pub is_business_bot_paused: bool,
    pub can_business_bot_reply: bool,
}

impl BusinessBotManageBar {
    /// `BusinessBotManageBar::store` (BusinessBotManageBar.h:46-61).
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_business_bot_user_id = self.business_bot_user_id.is_valid();
        let has_business_bot_manage_url = !self.business_bot_manage_url.is_empty();
        store_flags(
            out,
            [
                self.is_business_bot_paused,
                self.can_business_bot_reply,
                has_business_bot_user_id,
                has_business_bot_manage_url,
            ],
        );
        if has_business_bot_user_id {
            // C++ `td::store(UserId)` = `storer.store_long(id)` (UserId.h:53-55);
            // the legacy Support64BitIds version gate rides the enclosing
            // Message/Dialog parse (M.10.2 blob layer).
            store_i64(out, self.business_bot_user_id.get());
        }
        if has_business_bot_manage_url {
            store_string(out, &self.business_bot_manage_url);
        }
    }

    /// `BusinessBotManageBar::parse` (BusinessBotManageBar.h:63-79).
    pub fn parse_value(input: &[u8]) -> Result<BusinessBotManageBar, &'static str> {
        let mut parser = Parser::new(input);
        let mut result = BusinessBotManageBar::default();
        let [
            is_business_bot_paused,
            can_business_bot_reply,
            has_business_bot_user_id,
            has_business_bot_manage_url,
        ] = parser.parse_flags::<4>()?;
        result.is_business_bot_paused = is_business_bot_paused;
        result.can_business_bot_reply = can_business_bot_reply;
        if has_business_bot_user_id {
            let user_id = parser.i64_()?;
            result.business_bot_user_id = UserId::new(user_id);
        }
        if has_business_bot_manage_url {
            result.business_bot_manage_url = parser.string()?;
        }
        parser.fetch_end()?;
        Ok(result)
    }
}

/// `store(const ReactionType &, ...)` (ReactionType.hpp:17-21) — the raw
/// string; the C++ `CHECK(!is_empty())` is the caller's contract.
pub fn store_reaction_type(out: &mut Vec<u8>, reaction_type: &ReactionType) {
    store_string(out, reaction_type.get_string());
}

/// `parse(ReactionType &, ...)` (ReactionType.hpp:23-27).
pub fn parse_reaction_type(parser: &mut Parser) -> Result<ReactionType, &'static str> {
    Ok(ReactionType::new(parser.string()?))
}

/// `store(const MinChannel &, ...)` (MinChannel.hpp:17-41). RECORDED GAP:
/// the `MinChannel` stand-in has no `photo`/`accent_color_id` yet (the
/// chat-photo plane owns them), so `has_photo`/`has_accent_color_id` are
/// always stored false — correct for everything the Rust client writes —
/// and a parse of foreign data that sets either flag is an error rather
/// than a silent misread.
pub fn store_min_channel(out: &mut Vec<u8>, min_channel: &MinChannel) {
    let has_title = !min_channel.title.is_empty();
    store_flags(out, [has_title, false, min_channel.is_megagroup, false]);
    if has_title {
        store_string(out, &min_channel.title);
    }
}

/// `parse(MinChannel &, ...)` (MinChannel.hpp:43-67).
pub fn parse_min_channel(parser: &mut Parser) -> Result<MinChannel, &'static str> {
    let [has_title, has_photo, is_megagroup, has_accent_color_id] = parser.parse_flags::<4>()?;
    if has_photo || has_accent_color_id {
        // the MinChannel stand-in cannot consume the photo/accent payloads
        return Err("MinChannel photo/accent color is not supported yet");
    }
    let title = if has_title {
        parser.string()?
    } else {
        String::new()
    };
    Ok(MinChannel {
        title,
        is_megagroup,
    })
}

/// The C++ `td::store(const vector<T> &, ...)` framing — int32 length then
/// the elements (tl_helpers.h).
fn store_vec_header(out: &mut Vec<u8>, len: usize) {
    store_i32(out, len as i32);
}

pub(crate) fn store_dialog_id_vec(out: &mut Vec<u8>, values: &[DialogId]) {
    store_vec_header(out, values.len());
    for value in values {
        value.store(out);
    }
}

pub(crate) fn parse_dialog_id_vec(parser: &mut Parser) -> Result<Vec<DialogId>, &'static str> {
    let size = parser.i32_()?;
    if size < 0 || size as usize > parser.left_len() / 4 {
        return Err("Wrong vector length");
    }
    let mut values = Vec::with_capacity(size as usize);
    for _ in 0..size {
        values.push(DialogId::parse_from(parser)?);
    }
    Ok(values)
}

pub(crate) fn store_reaction_type_vec(out: &mut Vec<u8>, values: &[ReactionType]) {
    store_vec_header(out, values.len());
    for value in values {
        store_reaction_type(out, value);
    }
}

pub(crate) fn parse_reaction_type_vec(
    parser: &mut Parser,
) -> Result<Vec<ReactionType>, &'static str> {
    let size = parser.i32_()?;
    if size < 0 || size as usize > parser.left_len() {
        return Err("Wrong vector length");
    }
    let mut values = Vec::with_capacity(size as usize);
    for _ in 0..size {
        values.push(parse_reaction_type(parser)?);
    }
    Ok(values)
}

pub(crate) fn store_min_channel_vec(out: &mut Vec<u8>, values: &[(ChannelId, MinChannel)]) {
    store_vec_header(out, values.len());
    for (channel_id, min_channel) in values {
        channel_id.store(out);
        store_min_channel(out, min_channel);
    }
}

pub(crate) fn parse_min_channel_vec(
    parser: &mut Parser,
) -> Result<Vec<(ChannelId, MinChannel)>, &'static str> {
    let size = parser.i32_()?;
    if size < 0 || size as usize > parser.left_len() {
        return Err("Wrong vector length");
    }
    let mut values = Vec::with_capacity(size as usize);
    for _ in 0..size {
        let channel_id = ChannelId::parse_from(parser)?;
        let min_channel = parse_min_channel(parser)?;
        values.push((channel_id, min_channel));
    }
    Ok(values)
}

pub(crate) fn store_message_reaction_vec(out: &mut Vec<u8>, values: &[MessageReaction]) {
    store_vec_header(out, values.len());
    for value in values {
        value.store(out);
    }
}

pub(crate) fn parse_message_reaction_vec(
    parser: &mut Parser,
) -> Result<Vec<MessageReaction>, &'static str> {
    let size = parser.i32_()?;
    if size < 0 || size as usize > parser.left_len() {
        return Err("Wrong vector length");
    }
    let mut values = Vec::with_capacity(size as usize);
    for _ in 0..size {
        values.push(MessageReaction::parse_from(parser)?);
    }
    Ok(values)
}

pub(crate) fn store_unread_message_reaction_vec(
    out: &mut Vec<u8>,
    values: &[UnreadMessageReaction],
) {
    store_vec_header(out, values.len());
    for value in values {
        value.store(out);
    }
}

pub(crate) fn parse_unread_message_reaction_vec(
    parser: &mut Parser,
) -> Result<Vec<UnreadMessageReaction>, &'static str> {
    let size = parser.i32_()?;
    if size < 0 || size as usize > parser.left_len() {
        return Err("Wrong vector length");
    }
    let mut values = Vec::with_capacity(size as usize);
    for _ in 0..size {
        values.push(UnreadMessageReaction::parse_from(parser)?);
    }
    Ok(values)
}

pub(crate) fn store_message_reactor_vec(out: &mut Vec<u8>, values: &[MessageReactor]) {
    store_vec_header(out, values.len());
    for value in values {
        value.store(out);
    }
}

pub(crate) fn parse_message_reactor_vec(
    parser: &mut Parser,
) -> Result<Vec<MessageReactor>, &'static str> {
    let size = parser.i32_()?;
    if size < 0 || size as usize > parser.left_len() {
        return Err("Wrong vector length");
    }
    let mut values = Vec::with_capacity(size as usize);
    for _ in 0..size {
        values.push(MessageReactor::parse_from(parser)?);
    }
    Ok(values)
}

/// `td::MessageReaction` (MessageReaction.h:23-118) — one reaction type on a
/// message with its counter and recent choosers. Only the value plane +
/// store/parse land here (M.10.2); the mutation family (`set_as_chosen`,
/// `update_from`, ...) is owned by the Wave-2 ReactionManager slice.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MessageReaction {
    pub reaction_type: ReactionType,
    pub choose_count: i32,
    pub is_chosen: bool,
    pub my_recent_chooser_dialog_id: DialogId,
    pub recent_chooser_dialog_ids: Vec<DialogId>,
    pub recent_chooser_min_channels: Vec<(ChannelId, MinChannel)>,
}

impl MessageReaction {
    /// `MessageReaction::MAX_CHOOSE_COUNT` (MessageReaction.h:25).
    pub const MAX_CHOOSE_COUNT: i32 = 2147483640;

    /// `MessageReaction::is_empty` (MessageReaction.h:51-53).
    pub fn is_empty(&self) -> bool {
        self.choose_count <= 0
    }

    /// `MessageReaction::is_chosen` (MessageReaction.h:55-57).
    pub fn is_chosen(&self) -> bool {
        self.is_chosen
    }

    /// `MessageReaction::get_choose_count` (MessageReaction.h:89-91).
    pub fn get_choose_count(&self) -> i32 {
        self.choose_count
    }

    /// `MessageReaction::get_reaction_type` (MessageReaction.h:104-106).
    pub fn get_reaction_type(&self) -> &ReactionType {
        &self.reaction_type
    }

    /// `MessageReaction::fix_choose_count` (MessageReaction.cpp) — the
    /// counter is at least the number of recent choosers.
    pub fn fix_choose_count(&mut self) {
        self.choose_count = self
            .choose_count
            .max(self.recent_chooser_dialog_ids.len() as i32);
    }

    /// `MessageReaction::store` (MessageReaction.hpp:20-42).
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_recent_chooser_dialog_ids = !self.recent_chooser_dialog_ids.is_empty();
        let has_recent_chooser_min_channels = !self.recent_chooser_min_channels.is_empty();
        let has_my_recent_chooser_dialog_id = self.my_recent_chooser_dialog_id.is_valid();
        store_flags(
            out,
            [
                self.is_chosen,
                has_recent_chooser_dialog_ids,
                has_recent_chooser_min_channels,
                has_my_recent_chooser_dialog_id,
            ],
        );
        store_reaction_type(out, &self.reaction_type);
        store_i32(out, self.choose_count);
        if has_recent_chooser_dialog_ids {
            store_dialog_id_vec(out, &self.recent_chooser_dialog_ids);
        }
        if has_recent_chooser_min_channels {
            store_min_channel_vec(out, &self.recent_chooser_min_channels);
        }
        if has_my_recent_chooser_dialog_id {
            self.my_recent_chooser_dialog_id.store(out);
        }
    }

    /// `MessageReaction::parse` (MessageReaction.hpp:44-79).
    pub fn parse_from(parser: &mut Parser) -> Result<MessageReaction, &'static str> {
        let [
            is_chosen,
            has_recent_chooser_dialog_ids,
            has_recent_chooser_min_channels,
            has_my_recent_chooser_dialog_id,
        ] = parser.parse_flags::<4>()?;
        let reaction_type = parse_reaction_type(parser)?;
        let choose_count = parser.i32_()?;
        let recent_chooser_dialog_ids = if has_recent_chooser_dialog_ids {
            parse_dialog_id_vec(parser)?
        } else {
            Vec::new()
        };
        let recent_chooser_min_channels = if has_recent_chooser_min_channels {
            parse_min_channel_vec(parser)?
        } else {
            Vec::new()
        };
        let my_recent_chooser_dialog_id = if has_my_recent_chooser_dialog_id {
            let dialog_id = DialogId::parse_from(parser)?;
            if !dialog_id.is_valid() || !recent_chooser_dialog_ids.contains(&dialog_id) {
                return Err("Invalid recent reaction chooser");
            }
            dialog_id
        } else {
            DialogId::default()
        };
        let mut result = MessageReaction {
            reaction_type,
            choose_count,
            is_chosen,
            my_recent_chooser_dialog_id,
            recent_chooser_dialog_ids,
            recent_chooser_min_channels,
        };
        result.fix_choose_count();
        if result.is_empty() || result.reaction_type.is_empty() {
            return Err("Invalid message reaction");
        }
        Ok(result)
    }
}

/// `td::UnreadMessageReaction` (MessageReaction.h:122-150).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UnreadMessageReaction {
    pub reaction_type: ReactionType,
    pub sender_dialog_id: DialogId,
    pub is_big: bool,
}

impl UnreadMessageReaction {
    /// `UnreadMessageReaction::store` (MessageReaction.hpp:81-87).
    pub fn store(&self, out: &mut Vec<u8>) {
        store_flags(out, [self.is_big]);
        store_reaction_type(out, &self.reaction_type);
        self.sender_dialog_id.store(out);
    }

    /// `UnreadMessageReaction::parse` (MessageReaction.hpp:89-99).
    pub fn parse_from(parser: &mut Parser) -> Result<UnreadMessageReaction, &'static str> {
        let [is_big] = parser.parse_flags::<1>()?;
        let reaction_type = parse_reaction_type(parser)?;
        let sender_dialog_id = DialogId::parse_from(parser)?;
        if reaction_type.is_empty() {
            return Err("Invalid unread message reaction");
        }
        Ok(UnreadMessageReaction {
            reaction_type,
            sender_dialog_id,
            is_big,
        })
    }
}

/// `td::MessageReactor` (MessageReactor.h) — a top reactor of a message.
/// The `min_channel` photo/accent-color fields ride the `MinChannel`
/// stand-in's recorded gap.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MessageReactor {
    pub dialog_id: DialogId,
    pub min_channel: Option<MinChannel>,
    pub count: i32,
    pub is_top: bool,
    pub is_me: bool,
    pub is_anonymous: bool,
}

impl MessageReactor {
    /// `MessageReactor::store` (MessageReactor.hpp:17-38).
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_dialog_id = self.dialog_id.is_valid();
        let has_min_channel = self.min_channel.is_some();
        store_flags(
            out,
            [
                self.is_top,
                self.is_me,
                self.is_anonymous,
                has_dialog_id,
                has_min_channel,
            ],
        );
        if has_dialog_id {
            self.dialog_id.store(out);
        }
        store_i32(out, self.count);
        if has_min_channel {
            store_min_channel(out, self.min_channel.as_ref().unwrap());
        }
    }

    /// `MessageReactor::parse` (MessageReactor.hpp:40-61).
    pub fn parse_from(parser: &mut Parser) -> Result<MessageReactor, &'static str> {
        let [is_top, is_me, is_anonymous, has_dialog_id, has_min_channel] =
            parser.parse_flags::<5>()?;
        let dialog_id = if has_dialog_id {
            DialogId::parse_from(parser)?
        } else {
            DialogId::default()
        };
        let count = parser.i32_()?;
        let min_channel = if has_min_channel {
            Some(parse_min_channel(parser)?)
        } else {
            None
        };
        Ok(MessageReactor {
            dialog_id,
            min_channel,
            count,
            is_top,
            is_me,
            is_anonymous,
        })
    }
}

/// `td::MessageReactions` (MessageReaction.h:163-257) — the reactions block
/// of a message. The value plane + store/parse land here (M.10.2 —
/// `Message::store`/`parse` is byte-parity-bound to it); the manager family
/// (`get_message_reactions`, `update_from`, polling, the td_api object)
/// stays with the Wave-2 ReactionManager slice. The pending-paid-reaction
/// fields are memory-only in C++ and are not stored.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MessageReactions {
    pub reactions: Vec<MessageReaction>,
    pub unread_reactions: Vec<UnreadMessageReaction>,
    pub chosen_reaction_order: Vec<ReactionType>,
    pub top_reactors: Vec<MessageReactor>,
    pub pending_paid_reactions: i32,
    pub is_min: bool,
    pub need_polling: bool,
    pub can_get_added_reactions: bool,
    pub are_tags: bool,
}

impl MessageReactions {
    /// `MessageReactions::are_empty` (MessageReaction.cpp).
    pub fn are_empty(&self) -> bool {
        self.reactions.is_empty()
            && self.unread_reactions.is_empty()
            && self.top_reactors.is_empty()
    }

    /// `MessageReactions::store` (MessageReaction.hpp:114-137).
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_reactions = !self.reactions.is_empty();
        let has_unread_reactions = !self.unread_reactions.is_empty();
        let has_chosen_reaction_order = !self.chosen_reaction_order.is_empty();
        let has_top_reactors = !self.top_reactors.is_empty();
        store_flags(
            out,
            [
                self.is_min,
                self.need_polling,
                self.can_get_added_reactions,
                has_unread_reactions,
                has_reactions,
                has_chosen_reaction_order,
                self.are_tags,
                has_top_reactors,
            ],
        );
        if has_reactions {
            store_message_reaction_vec(out, &self.reactions);
        }
        if has_unread_reactions {
            store_unread_message_reaction_vec(out, &self.unread_reactions);
        }
        if has_chosen_reaction_order {
            store_reaction_type_vec(out, &self.chosen_reaction_order);
        }
        if has_top_reactors {
            store_message_reactor_vec(out, &self.top_reactors);
        }
    }

    /// `MessageReactions::parse` (MessageReaction.hpp:139-160).
    pub fn parse_from(parser: &mut Parser) -> Result<MessageReactions, &'static str> {
        let [
            is_min,
            need_polling,
            can_get_added_reactions,
            has_unread_reactions,
            has_reactions,
            has_chosen_reaction_order,
            are_tags,
            has_top_reactors,
        ] = parser.parse_flags::<8>()?;
        let reactions = if has_reactions {
            parse_message_reaction_vec(parser)?
        } else {
            Vec::new()
        };
        let unread_reactions = if has_unread_reactions {
            parse_unread_message_reaction_vec(parser)?
        } else {
            Vec::new()
        };
        let chosen_reaction_order = if has_chosen_reaction_order {
            parse_reaction_type_vec(parser)?
        } else {
            Vec::new()
        };
        let top_reactors = if has_top_reactors {
            parse_message_reactor_vec(parser)?
        } else {
            Vec::new()
        };
        Ok(MessageReactions {
            reactions,
            unread_reactions,
            chosen_reaction_order,
            top_reactors,
            pending_paid_reactions: 0,
            is_min,
            need_polling,
            can_get_added_reactions,
            are_tags,
        })
    }
}

// ---------------------------------------------------------------------------
// M.10.2b — the internal ReplyMarkup plane (ReplyMarkup.h, KeyboardButton.h,
// InlineKeyboardButton.h, KeyboardButtonStyle.h, RequestedDialogType.h) with
// the store/parse formats (the matching .hpp files) and the
// telegram_api→internal converters (`get_reply_markup(telegram_api...)`,
// ReplyMarkup.cpp:37-135). The internal model is what `Message::reply_markup`
// holds (the M.10.1b stand-in re-points here); the td_api input/object
// converters land with the send path (M.10.8) and `get_message_object`
// (M.10.4).
// ---------------------------------------------------------------------------

/// `KeyboardButtonStyle::Type` (KeyboardButtonStyle.h:19).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(i32)]
pub enum KeyboardButtonStyleType {
    #[default]
    Default = 0,
    Primary = 1,
    Danger = 2,
    Success = 3,
}

/// `td::KeyboardButtonStyle` (KeyboardButtonStyle.h).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct KeyboardButtonStyle {
    pub r#type: KeyboardButtonStyleType,
    pub icon_custom_emoji_id: crate::emoji_status::CustomEmojiId,
}

impl KeyboardButtonStyle {
    /// `KeyboardButtonStyle::KeyboardButtonStyle(telegram_api::keyboardButtonStyle &&)`
    /// (KeyboardButtonStyle.cpp:37-45).
    pub fn from_telegram(style: Option<&telegram_api::KeyboardButtonStyle>) -> KeyboardButtonStyle {
        let Some(style) = style else {
            return KeyboardButtonStyle::default();
        };
        let r#type = if style.bg_primary {
            KeyboardButtonStyleType::Primary
        } else if style.bg_danger {
            KeyboardButtonStyleType::Danger
        } else if style.bg_success {
            KeyboardButtonStyleType::Success
        } else {
            KeyboardButtonStyleType::Default
        };
        KeyboardButtonStyle {
            r#type,
            icon_custom_emoji_id: crate::emoji_status::CustomEmojiId::new(style.icon),
        }
    }

    /// `KeyboardButtonStyle::is_default` (KeyboardButtonStyle.h).
    pub fn is_default(&self) -> bool {
        self.r#type == KeyboardButtonStyleType::Default && !self.icon_custom_emoji_id.is_valid()
    }

    /// `KeyboardButtonStyle::store` (KeyboardButtonStyle.hpp:14-31).
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_type = self.r#type != KeyboardButtonStyleType::Default;
        let has_icon_custom_emoji_id = self.icon_custom_emoji_id.is_valid();
        store_flags(out, [has_type, has_icon_custom_emoji_id]);
        if has_type {
            store_i32(out, self.r#type as i32);
        }
        if has_icon_custom_emoji_id {
            store_i64(out, self.icon_custom_emoji_id.get());
        }
    }

    /// `KeyboardButtonStyle::parse` (KeyboardButtonStyle.hpp:33-49).
    pub fn parse_from(parser: &mut Parser) -> Result<KeyboardButtonStyle, &'static str> {
        let [has_type, has_icon_custom_emoji_id] = parser.parse_flags::<2>()?;
        let r#type = if has_type {
            match parser.i32_()? {
                0 => KeyboardButtonStyleType::Default,
                1 => KeyboardButtonStyleType::Primary,
                2 => KeyboardButtonStyleType::Danger,
                3 => KeyboardButtonStyleType::Success,
                _ => return Err("Wrong keyboard button style type"),
            }
        } else {
            KeyboardButtonStyleType::Default
        };
        let icon_custom_emoji_id = if has_icon_custom_emoji_id {
            crate::emoji_status::CustomEmojiId::new(parser.i64_()?)
        } else {
            crate::emoji_status::CustomEmojiId::default()
        };
        Ok(KeyboardButtonStyle {
            r#type,
            icon_custom_emoji_id,
        })
    }
}

/// `RequestedDialogType::Type` (RequestedDialogType.h:22).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(i32)]
pub enum RequestedDialogTypeType {
    #[default]
    User = 0,
    Group = 1,
    Channel = 2,
    CreateBot = 3,
}

/// `td::RequestedDialogType` (RequestedDialogType.h:19-100).
#[derive(Clone, Debug, PartialEq)]
pub struct RequestedDialogType {
    pub r#type: RequestedDialogTypeType,
    pub button_id: i32,
    pub max_quantity: i32,
    pub restrict_is_bot: bool,
    pub is_bot: bool,
    pub restrict_is_premium: bool,
    pub is_premium: bool,
    pub request_name: bool,
    pub request_username: bool,
    pub request_photo: bool,
    pub restrict_is_forum: bool,
    pub is_forum: bool,
    pub bot_is_participant: bool,
    pub restrict_has_username: bool,
    pub has_username: bool,
    pub is_created: bool,
    pub restrict_user_administrator_rights: bool,
    pub restrict_bot_administrator_rights: bool,
    pub user_administrator_rights: crate::chat_manager::AdministratorRights,
    pub bot_administrator_rights: crate::chat_manager::AdministratorRights,
    pub suggested_name: String,
    pub suggested_username: String,
}

impl Default for RequestedDialogType {
    /// The C++ field initializers (RequestedDialogType.h:23-45); the
    /// AdministratorRights store/parse lands beside this type in
    /// chat_manager.
    fn default() -> RequestedDialogType {
        RequestedDialogType {
            r#type: RequestedDialogTypeType::User,
            button_id: 0,
            max_quantity: 1,
            restrict_is_bot: false,
            is_bot: false,
            restrict_is_premium: false,
            is_premium: false,
            request_name: false,
            request_username: false,
            request_photo: false,
            restrict_is_forum: false,
            is_forum: false,
            bot_is_participant: false,
            restrict_has_username: false,
            has_username: false,
            is_created: false,
            restrict_user_administrator_rights: false,
            restrict_bot_administrator_rights: false,
            user_administrator_rights: crate::chat_manager::AdministratorRights::default(),
            bot_administrator_rights: crate::chat_manager::AdministratorRights::default(),
            suggested_name: String::new(),
            suggested_username: String::new(),
        }
    }
}

impl RequestedDialogType {
    /// `RequestedDialogType::RequestedDialogType(telegram_api::RequestPeerType &&, ...)`
    /// (RequestedDialogType.cpp:26-84).
    pub fn from_telegram(
        peer_type: telegram_api::RequestPeerType,
        button_id: i32,
        max_quantity: i32,
    ) -> RequestedDialogType {
        let mut result = RequestedDialogType {
            button_id,
            max_quantity: max_quantity.max(1),
            ..RequestedDialogType::default()
        };
        match peer_type {
            telegram_api::RequestPeerType::requestPeerTypeUser(t) => {
                result.r#type = RequestedDialogTypeType::User;
                result.restrict_is_bot = t.flags & 1 != 0;
                result.is_bot = t.bot;
                result.restrict_is_premium = t.flags & 2 != 0;
                result.is_premium = t.premium;
            }
            telegram_api::RequestPeerType::requestPeerTypeChat(t) => {
                result.r#type = RequestedDialogTypeType::Group;
                result.restrict_is_forum = t.flags & 16 != 0;
                result.is_forum = t.forum;
                result.bot_is_participant = t.bot_participant;
                result.restrict_has_username = t.flags & 8 != 0;
                result.has_username = t.has_username;
                result.is_created = t.creator;
                result.restrict_user_administrator_rights =
                    !result.is_created && t.user_admin_rights.is_some();
                result.restrict_bot_administrator_rights = t.bot_admin_rights.is_some();
                result.user_administrator_rights =
                    crate::chat_manager::AdministratorRights::from_chat_admin_rights(
                        t.user_admin_rights.as_ref(),
                        ChannelType::Megagroup,
                    );
                result.bot_administrator_rights =
                    crate::chat_manager::AdministratorRights::from_chat_admin_rights(
                        t.bot_admin_rights.as_ref(),
                        ChannelType::Megagroup,
                    );
            }
            telegram_api::RequestPeerType::requestPeerTypeBroadcast(t) => {
                result.r#type = RequestedDialogTypeType::Channel;
                result.restrict_has_username = t.flags & 8 != 0;
                result.has_username = t.has_username;
                result.is_created = t.creator;
                result.restrict_user_administrator_rights =
                    !result.is_created && t.user_admin_rights.is_some();
                result.restrict_bot_administrator_rights = t.bot_admin_rights.is_some();
                result.user_administrator_rights =
                    crate::chat_manager::AdministratorRights::from_chat_admin_rights(
                        t.user_admin_rights.as_ref(),
                        ChannelType::Broadcast,
                    );
                result.bot_administrator_rights =
                    crate::chat_manager::AdministratorRights::from_chat_admin_rights(
                        t.bot_admin_rights.as_ref(),
                        ChannelType::Broadcast,
                    );
            }
            telegram_api::RequestPeerType::requestPeerTypeCreateBot(t) => {
                if t.bot_managed {
                    result.r#type = RequestedDialogTypeType::CreateBot;
                    result.suggested_name = t.suggested_name;
                    result.suggested_username = t.suggested_username;
                } else {
                    log_error!("Receive request to create an unmanaged bot");
                    result.r#type = RequestedDialogTypeType::User;
                    result.restrict_is_bot = true;
                    result.is_bot = true;
                }
            }
        }
        result
    }

    /// `RequestedDialogType::store` (RequestedDialogType.hpp:17-57).
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_max_quantity = self.max_quantity != 1;
        let has_suggested_name = !self.suggested_name.is_empty();
        let has_suggested_username = !self.suggested_username.is_empty();
        store_flags(
            out,
            [
                self.restrict_is_bot,
                self.is_bot,
                self.restrict_is_premium,
                self.is_premium,
                self.restrict_is_forum,
                self.is_forum,
                self.bot_is_participant,
                self.restrict_has_username,
                self.has_username,
                self.is_created,
                self.restrict_user_administrator_rights,
                self.restrict_bot_administrator_rights,
                has_max_quantity,
                has_suggested_name,
                has_suggested_username,
            ],
        );
        store_i32(out, self.r#type as i32);
        store_i32(out, self.button_id);
        if self.restrict_user_administrator_rights {
            self.user_administrator_rights.store(out);
        }
        if self.restrict_bot_administrator_rights {
            self.bot_administrator_rights.store(out);
        }
        if has_max_quantity {
            store_i32(out, self.max_quantity);
        }
        if has_suggested_name {
            store_string(out, &self.suggested_name);
        }
        if has_suggested_username {
            store_string(out, &self.suggested_username);
        }
    }

    /// `RequestedDialogType::parse` (RequestedDialogType.hpp:59-113).
    pub fn parse_from(parser: &mut Parser) -> Result<RequestedDialogType, &'static str> {
        let [
            restrict_is_bot,
            is_bot,
            restrict_is_premium,
            is_premium,
            restrict_is_forum,
            is_forum,
            bot_is_participant,
            restrict_has_username,
            has_username,
            is_created,
            restrict_user_administrator_rights,
            restrict_bot_administrator_rights,
            has_max_quantity,
            has_suggested_name,
            has_suggested_username,
        ] = parser.parse_flags::<15>()?;
        let r#type = match parser.i32_()? {
            0 => RequestedDialogTypeType::User,
            1 => RequestedDialogTypeType::Group,
            2 => RequestedDialogTypeType::Channel,
            3 => RequestedDialogTypeType::CreateBot,
            _ => return Err("Wrong requested dialog type"),
        };
        let button_id = parser.i32_()?;
        let user_administrator_rights = if restrict_user_administrator_rights {
            crate::chat_manager::AdministratorRights::parse_impl(parser)?
        } else {
            crate::chat_manager::AdministratorRights::default()
        };
        let bot_administrator_rights = if restrict_bot_administrator_rights {
            crate::chat_manager::AdministratorRights::parse_impl(parser)?
        } else {
            crate::chat_manager::AdministratorRights::default()
        };
        let max_quantity = if has_max_quantity { parser.i32_()? } else { 1 };
        let suggested_name = if has_suggested_name {
            parser.string()?
        } else {
            String::new()
        };
        let suggested_username = if has_suggested_username {
            parser.string()?
        } else {
            String::new()
        };
        Ok(RequestedDialogType {
            r#type,
            button_id,
            max_quantity,
            restrict_is_bot,
            is_bot,
            restrict_is_premium,
            is_premium,
            request_name: false,
            request_username: false,
            request_photo: false,
            restrict_is_forum,
            is_forum,
            bot_is_participant,
            restrict_has_username,
            has_username,
            is_created,
            restrict_user_administrator_rights,
            restrict_bot_administrator_rights,
            user_administrator_rights,
            bot_administrator_rights,
            suggested_name,
            suggested_username,
        })
    }
}

/// `KeyboardButton::Type` (KeyboardButton.h:25-35) — append only.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(i32)]
pub enum KeyboardButtonType {
    #[default]
    Text = 0,
    RequestPhoneNumber = 1,
    RequestLocation = 2,
    RequestPoll = 3,
    RequestPollQuiz = 4,
    RequestPollRegular = 5,
    WebView = 6,
    RequestDialog = 7,
}

/// `td::KeyboardButton` (KeyboardButton.h:23-52).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct KeyboardButton {
    pub r#type: KeyboardButtonType,
    pub style: KeyboardButtonStyle,
    pub text: String,
    /// WebView only
    pub url: String,
    /// RequestDialog only
    pub requested_dialog_type: Option<RequestedDialogType>,
}

impl KeyboardButton {
    /// `KeyboardButton::KeyboardButton(telegram_api::keyboardButton &&)`
    /// (KeyboardButton.cpp:22-79).
    pub fn from_telegram(keyboard_button: telegram_api::KeyboardButton) -> KeyboardButton {
        let mut result = KeyboardButton::default();
        let Some(button_type) = keyboard_button.r#type else {
            log_error!("Unsupported keyboard button");
            result.style = KeyboardButtonStyle::from_telegram(keyboard_button.style.as_ref());
            result.text = keyboard_button.text;
            return result;
        };
        match button_type {
            telegram_api::ButtonType::buttonTypeDefault(_) => {
                result.r#type = KeyboardButtonType::Text;
            }
            telegram_api::ButtonType::buttonTypeRequestPhone(_) => {
                result.r#type = KeyboardButtonType::RequestPhoneNumber;
            }
            telegram_api::ButtonType::buttonTypeRequestGeoLocation(_) => {
                result.r#type = KeyboardButtonType::RequestLocation;
            }
            telegram_api::ButtonType::buttonTypeRequestPoll(t) => {
                if t.flags & 1 != 0 {
                    result.r#type = if t.quiz {
                        KeyboardButtonType::RequestPollQuiz
                    } else {
                        KeyboardButtonType::RequestPollRegular
                    };
                } else {
                    result.r#type = KeyboardButtonType::RequestPoll;
                }
            }
            telegram_api::ButtonType::buttonTypeSimpleWebView(t) => {
                match crate::link_manager::check_link(&t.url, false, false) {
                    Ok(url) => {
                        result.r#type = KeyboardButtonType::WebView;
                        result.url = url;
                    }
                    Err(error) => {
                        log_error!("Keyboard Web App {}", error.message());
                    }
                }
            }
            telegram_api::ButtonType::buttonTypeRequestPeer(t) => {
                result.r#type = KeyboardButtonType::RequestDialog;
                if let Some(peer_type) = t.peer_type {
                    result.requested_dialog_type = Some(RequestedDialogType::from_telegram(
                        peer_type,
                        t.button_id,
                        t.max_quantity,
                    ));
                }
            }
            telegram_api::ButtonType::inputButtonTypeRequestPeer(_) => {
                log_error!("Unsupported keyboard button");
            }
        }
        result.style = KeyboardButtonStyle::from_telegram(keyboard_button.style.as_ref());
        result.text = keyboard_button.text;
        result
    }

    /// `KeyboardButton::is_empty` (KeyboardButton.h:45-47).
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
            && (self.r#type == KeyboardButtonType::Text
                || !self.style.icon_custom_emoji_id.is_valid())
    }

    /// `KeyboardButton::store` (KeyboardButton.hpp:17-40).
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_url = !self.url.is_empty();
        let has_requested_dialog_type = self.requested_dialog_type.is_some();
        let has_style = !self.style.is_default();
        store_flags(out, [has_url, has_requested_dialog_type, has_style]);
        store_i32(out, self.r#type as i32);
        store_string(out, &self.text);
        if has_url {
            store_string(out, &self.url);
        }
        if has_requested_dialog_type {
            self.requested_dialog_type.as_ref().unwrap().store(out);
        }
        if has_style {
            self.style.store(out);
        }
    }

    /// `KeyboardButton::parse` (KeyboardButton.hpp:42-80) — the
    /// pre-`AddKeyboardButtonFlags` (35) legacy layout stores type/text/url
    /// unconditionally.
    pub fn parse_from(parser: &mut Parser) -> Result<KeyboardButton, &'static str> {
        let mut result = KeyboardButton::default();
        let has_url;
        let has_requested_dialog_type;
        let has_style;
        if parser.version() >= crate::store_parse::version::ADD_KEYBOARD_BUTTON_FLAGS {
            let [has_url_, has_requested_dialog_type_, has_style_] = parser.parse_flags::<3>()?;
            has_url = has_url_;
            has_requested_dialog_type = has_requested_dialog_type_;
            has_style = has_style_;
        } else {
            has_url = false;
            has_requested_dialog_type = false;
            has_style = false;
        }
        result.r#type = match parser.i32_()? {
            0 => KeyboardButtonType::Text,
            1 => KeyboardButtonType::RequestPhoneNumber,
            2 => KeyboardButtonType::RequestLocation,
            3 => KeyboardButtonType::RequestPoll,
            4 => KeyboardButtonType::RequestPollQuiz,
            5 => KeyboardButtonType::RequestPollRegular,
            6 => KeyboardButtonType::WebView,
            7 => KeyboardButtonType::RequestDialog,
            _ => return Err("Wrong keyboard button type"),
        };
        result.text = parser.string()?;
        if has_url {
            result.url = parser.string()?;
        }
        if has_requested_dialog_type {
            result.requested_dialog_type = Some(RequestedDialogType::parse_from(parser)?);
        }
        if has_style {
            result.style = KeyboardButtonStyle::parse_from(parser)?;
        }
        Ok(result)
    }
}

/// `InlineKeyboardButton::Type` (InlineKeyboardButton.h:23-38) — append only.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(i32)]
pub enum InlineKeyboardButtonType {
    Url = 0,
    Callback = 1,
    CallbackGame = 2,
    SwitchInline = 3,
    SwitchInlineCurrentDialog = 4,
    Buy = 5,
    UrlAuth = 6,
    CallbackWithPassword = 7,
    User = 8,
    WebView = 9,
    Copy = 10,
    #[default]
    Disabled = 11,
}

/// `td::InlineKeyboardButton` (InlineKeyboardButton.h:21-58).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct InlineKeyboardButton {
    pub r#type: InlineKeyboardButtonType,
    /// UrlAuth: button id; SwitchInline: target dialog types mask
    pub id: i64,
    /// User only
    pub user_id: UserId,
    pub style: KeyboardButtonStyle,
    pub text: String,
    /// UrlAuth only
    pub forward_text: String,
    pub data: String,
}

impl InlineKeyboardButton {
    /// `get_inline_keyboard_button(telegram_api::keyboardInlineButton &&)`
    /// (InlineKeyboardButton.cpp:126-204). Invalid links/users leave the
    /// button Disabled like the C++ `break`-out-of-switch.
    pub fn from_telegram(
        keyboard_button: telegram_api::KeyboardInlineButton,
    ) -> InlineKeyboardButton {
        let mut button = InlineKeyboardButton::default();
        let Some(button_type) = keyboard_button.r#type else {
            log_error!("Unsupported inline keyboard button");
            button.style = KeyboardButtonStyle::from_telegram(keyboard_button.style.as_ref());
            button.text = keyboard_button.text;
            return button;
        };
        match button_type {
            telegram_api::InlineButtonType::inlineButtonTypeUrl(t) => {
                match crate::link_manager::check_link(&t.url, false, false) {
                    Ok(url) => {
                        button.r#type = InlineKeyboardButtonType::Url;
                        button.data = url;
                    }
                    Err(error) => log_error!("Inline keyboard {}", error.message()),
                }
            }
            telegram_api::InlineButtonType::inlineButtonTypeCallback(t) => {
                button.r#type = if t.requires_password {
                    InlineKeyboardButtonType::CallbackWithPassword
                } else {
                    InlineKeyboardButtonType::Callback
                };
                button.data = String::from_utf8_lossy(&t.data).into_owned();
            }
            telegram_api::InlineButtonType::inlineButtonTypeGame(_) => {
                button.r#type = InlineKeyboardButtonType::CallbackGame;
            }
            telegram_api::InlineButtonType::inlineButtonTypeSwitchInline(t) => {
                if t.same_peer {
                    button.r#type = InlineKeyboardButtonType::SwitchInlineCurrentDialog;
                } else {
                    button.r#type = InlineKeyboardButtonType::SwitchInline;
                    button.id = get_inline_query_peer_types_mask(&t.peer_types);
                }
                button.data = t.query;
            }
            telegram_api::InlineButtonType::inlineButtonTypeBuy(_) => {
                button.r#type = InlineKeyboardButtonType::Buy;
            }
            telegram_api::InlineButtonType::inlineButtonTypeUrlAuth(t) => {
                match crate::link_manager::check_link(&t.url, false, false) {
                    Ok(url) => {
                        button.r#type = InlineKeyboardButtonType::UrlAuth;
                        button.id = t.button_id as i64;
                        button.forward_text = t.fwd_text;
                        button.data = url;
                    }
                    Err(error) => log_error!("Inline keyboard Login {}", error.message()),
                }
            }
            telegram_api::InlineButtonType::inlineButtonTypeUserProfile(t) => {
                let user_id = UserId::new(t.user_id);
                if !user_id.is_valid() {
                    log_error!("Receive {} in inline keyboard", user_id.get());
                } else {
                    button.r#type = InlineKeyboardButtonType::User;
                    button.user_id = user_id;
                }
            }
            telegram_api::InlineButtonType::inlineButtonTypeWebView(t) => {
                match crate::link_manager::check_link(&t.url, false, false) {
                    Ok(url) => {
                        button.r#type = InlineKeyboardButtonType::WebView;
                        button.data = url;
                    }
                    Err(error) => log_error!("Inline keyboard Web App {}", error.message()),
                }
            }
            telegram_api::InlineButtonType::inlineButtonTypeCopy(t) => {
                button.r#type = InlineKeyboardButtonType::Copy;
                button.data = t.copy_text;
            }
            telegram_api::InlineButtonType::inlineButtonTypeDisabled(_) => {
                button.r#type = InlineKeyboardButtonType::Disabled;
            }
            telegram_api::InlineButtonType::inputInlineButtonTypeUrlAuth(_)
            | telegram_api::InlineButtonType::inputInlineButtonTypeUserProfile(_) => {
                log_error!("Unsupported inline keyboard button");
            }
        }
        button.style = KeyboardButtonStyle::from_telegram(keyboard_button.style.as_ref());
        button.text = keyboard_button.text;
        button
    }

    /// `InlineKeyboardButton::store` (InlineKeyboardButton.hpp:17-51).
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_id = self.id != 0;
        let has_user_id = self.user_id.is_valid();
        let has_forward_text = !self.forward_text.is_empty();
        let has_data = !self.data.is_empty();
        let has_style = !self.style.is_default();
        store_flags(
            out,
            [has_id, has_user_id, has_forward_text, has_data, has_style],
        );
        store_i32(out, self.r#type as i32);
        if has_id {
            store_i64(out, self.id);
        }
        if has_user_id {
            self.user_id.store(out);
        }
        store_string(out, &self.text);
        if has_forward_text {
            store_string(out, &self.forward_text);
        }
        if has_data {
            store_string(out, &self.data);
        }
        if has_style {
            self.style.store(out);
        }
    }

    /// `InlineKeyboardButton::parse` (InlineKeyboardButton.hpp:53-98) — with
    /// the pre-`AddKeyboardButtonFlags` legacy layout.
    pub fn parse_from(parser: &mut Parser) -> Result<InlineKeyboardButton, &'static str> {
        let mut button = InlineKeyboardButton::default();
        if parser.version() >= crate::store_parse::version::ADD_KEYBOARD_BUTTON_FLAGS {
            let [has_id, has_user_id, has_forward_text, has_data, has_style] =
                parser.parse_flags::<5>()?;
            button.r#type = parse_inline_keyboard_button_type(parser)?;
            if has_id {
                button.id = parser.i64_()?;
            }
            if has_user_id {
                button.user_id = UserId::parse_from(parser)?;
            }
            button.text = parser.string()?;
            if has_forward_text {
                button.forward_text = parser.string()?;
            }
            if has_data {
                button.data = parser.string()?;
            }
            if has_style {
                button.style = KeyboardButtonStyle::parse_from(parser)?;
            }
        } else {
            button.r#type = parse_inline_keyboard_button_type(parser)?;
            if button.r#type == InlineKeyboardButtonType::UrlAuth {
                if parser.version() >= crate::store_parse::version::SUPPORT_64_BIT_IDS {
                    button.id = parser.i64_()?;
                } else {
                    button.id = parser.i32_()? as i64;
                }
            }
            button.text = parser.string()?;
            button.data = parser.string()?;
        }
        Ok(button)
    }
}

fn parse_inline_keyboard_button_type(
    parser: &mut Parser,
) -> Result<InlineKeyboardButtonType, &'static str> {
    Ok(match parser.i32_()? {
        0 => InlineKeyboardButtonType::Url,
        1 => InlineKeyboardButtonType::Callback,
        2 => InlineKeyboardButtonType::CallbackGame,
        3 => InlineKeyboardButtonType::SwitchInline,
        4 => InlineKeyboardButtonType::SwitchInlineCurrentDialog,
        5 => InlineKeyboardButtonType::Buy,
        6 => InlineKeyboardButtonType::UrlAuth,
        7 => InlineKeyboardButtonType::CallbackWithPassword,
        8 => InlineKeyboardButtonType::User,
        9 => InlineKeyboardButtonType::WebView,
        10 => InlineKeyboardButtonType::Copy,
        11 => InlineKeyboardButtonType::Disabled,
        _ => return Err("Wrong inline keyboard button type"),
    })
}

/// `TargetDialogTypes(vector<InlineQueryPeerType> &&)` (TargetDialogTypes.cpp:14-35)
/// — the USERS/BOTS/CHATS/BROADCASTS bit mask.
pub fn get_inline_query_peer_types_mask(peer_types: &[telegram_api::InlineQueryPeerType]) -> i64 {
    const USERS_MASK: i64 = 1;
    const BOTS_MASK: i64 = 2;
    const CHATS_MASK: i64 = 4;
    const BROADCASTS_MASK: i64 = 8;
    let mut mask = 0i64;
    for peer_type in peer_types {
        mask |= match peer_type {
            telegram_api::InlineQueryPeerType::inlineQueryPeerTypePM(_) => USERS_MASK,
            telegram_api::InlineQueryPeerType::inlineQueryPeerTypeBotPM(_) => BOTS_MASK,
            telegram_api::InlineQueryPeerType::inlineQueryPeerTypeChat(_)
            | telegram_api::InlineQueryPeerType::inlineQueryPeerTypeMegagroup(_) => CHATS_MASK,
            telegram_api::InlineQueryPeerType::inlineQueryPeerTypeBroadcast(_) => BROADCASTS_MASK,
            telegram_api::InlineQueryPeerType::inlineQueryPeerTypeSameBotPM(_) => {
                log_error!("Receive inlineQueryPeerTypeSameBotPM");
                0
            }
        };
    }
    mask
}

/// `ReplyMarkup::Type` (ReplyMarkup.h:27) — append only.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(i32)]
pub enum ReplyMarkupType {
    InlineKeyboard = 0,
    ShowKeyboard = 1,
    RemoveKeyboard = 2,
    #[default]
    ForceReply = 3,
}

/// `td::ReplyMarkup` (ReplyMarkup.h:25-49) — the internal reply markup a
/// `Message` holds. The M.10.1b stand-in typed this as the generated
/// `telegram_api::ReplyMarkup`; M.10.2b re-points `Message::reply_markup`
/// here and keeps the wire conversion in [`ReplyMarkup::from_telegram`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ReplyMarkup {
    pub r#type: ReplyMarkupType,
    /// for ShowKeyboard, RemoveKeyboard, ForceReply
    pub is_personal: bool,
    /// for ShowKeyboard, InlineKeyboard
    pub force_reply: bool,
    /// for ShowKeyboard
    pub is_persistent: bool,
    /// for ShowKeyboard
    pub need_resize_keyboard: bool,
    /// for ShowKeyboard
    pub is_one_time_keyboard: bool,
    /// for ShowKeyboard
    pub keyboard: Vec<Vec<KeyboardButton>>,
    /// for ShowKeyboard, ForceReply
    pub placeholder: String,
    /// for InlineKeyboard
    pub inline_keyboard: Vec<Vec<InlineKeyboardButton>>,
}

impl ReplyMarkup {
    /// `get_reply_markup(telegram_api::ReplyMarkup &&, ...)` (ReplyMarkup.cpp:37-135).
    pub fn from_telegram(
        reply_markup: telegram_api::ReplyMarkup,
        only_inline_keyboard: bool,
        is_bot: bool,
        message_contains_mention: bool,
    ) -> Option<ReplyMarkup> {
        let mut result = ReplyMarkup::default();
        match reply_markup {
            telegram_api::ReplyMarkup::replyInlineMarkup(inline_markup) => {
                result.r#type = ReplyMarkupType::InlineKeyboard;
                for row in &inline_markup.rows {
                    let mut buttons = Vec::new();
                    for button in &row.buttons {
                        let button = InlineKeyboardButton::from_telegram(button.clone());
                        if !button.text.is_empty() || button.style.icon_custom_emoji_id.is_valid() {
                            buttons.push(button);
                        }
                    }
                    if !buttons.is_empty() {
                        result.inline_keyboard.push(buttons);
                    }
                }
                if result.inline_keyboard.is_empty() {
                    return None;
                }
                result.force_reply = inline_markup.force_reply;
            }
            telegram_api::ReplyMarkup::replyKeyboardMarkup(keyboard_markup) => {
                if only_inline_keyboard {
                    log_error!("Inline keyboard expected");
                    return None;
                }
                result.r#type = ReplyMarkupType::ShowKeyboard;
                result.is_persistent = keyboard_markup.persistent;
                result.need_resize_keyboard = keyboard_markup.resize;
                result.is_one_time_keyboard = keyboard_markup.single_use;
                result.is_personal = keyboard_markup.selective;
                result.force_reply = keyboard_markup.force_reply;
                result.placeholder = keyboard_markup.placeholder;
                for row in &keyboard_markup.rows {
                    let mut buttons = Vec::new();
                    for button in &row.buttons {
                        let button = KeyboardButton::from_telegram(button.clone());
                        if !button.is_empty() {
                            buttons.push(button);
                        }
                    }
                    if !buttons.is_empty() {
                        result.keyboard.push(buttons);
                    }
                }
                if result.keyboard.is_empty() {
                    return None;
                }
            }
            telegram_api::ReplyMarkup::replyKeyboardHide(hide_keyboard_markup) => {
                if only_inline_keyboard {
                    log_error!("Inline keyboard expected");
                    return None;
                }
                result.r#type = ReplyMarkupType::RemoveKeyboard;
                result.is_personal = hide_keyboard_markup.selective;
            }
            telegram_api::ReplyMarkup::replyKeyboardForceReply(force_reply_markup) => {
                if only_inline_keyboard {
                    log_error!("Inline keyboard expected");
                    return None;
                }
                result.r#type = ReplyMarkupType::ForceReply;
                result.is_personal = force_reply_markup.selective;
                result.placeholder = force_reply_markup.placeholder;
            }
        }

        if !is_bot && result.r#type != ReplyMarkupType::InlineKeyboard {
            // incoming keyboard
            if result.is_personal {
                if !message_contains_mention {
                    result.is_personal = false;
                    result.force_reply = false;
                }
            } else {
                result.is_personal = true;
            }
        }

        Some(result)
    }

    /// `store(const ReplyMarkup &, ...)` (ReplyMarkup.hpp:17-42).
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_keyboard = !self.keyboard.is_empty();
        let has_inline_keyboard = !self.inline_keyboard.is_empty();
        let has_placeholder = !self.placeholder.is_empty();
        store_flags(
            out,
            [
                self.is_personal,
                self.need_resize_keyboard,
                self.is_one_time_keyboard,
                has_keyboard,
                has_inline_keyboard,
                has_placeholder,
                self.is_persistent,
                self.force_reply,
            ],
        );
        store_i32(out, self.r#type as i32);
        if has_keyboard {
            store_button_rows(out, &self.keyboard);
        }
        if has_inline_keyboard {
            store_inline_button_rows(out, &self.inline_keyboard);
        }
        if has_placeholder {
            store_string(out, &self.placeholder);
        }
    }

    /// `parse(ReplyMarkup &, ...)` (ReplyMarkup.hpp:44-69).
    pub fn parse_from(parser: &mut Parser) -> Result<ReplyMarkup, &'static str> {
        let [
            is_personal,
            need_resize_keyboard,
            is_one_time_keyboard,
            has_keyboard,
            has_inline_keyboard,
            has_placeholder,
            is_persistent,
            force_reply,
        ] = parser.parse_flags::<8>()?;
        let r#type = match parser.i32_()? {
            0 => ReplyMarkupType::InlineKeyboard,
            1 => ReplyMarkupType::ShowKeyboard,
            2 => ReplyMarkupType::RemoveKeyboard,
            3 => ReplyMarkupType::ForceReply,
            _ => return Err("Wrong reply markup type"),
        };
        let keyboard = if has_keyboard {
            parse_button_rows(parser)?
        } else {
            Vec::new()
        };
        let inline_keyboard = if has_inline_keyboard {
            parse_inline_button_rows(parser)?
        } else {
            Vec::new()
        };
        let placeholder = if has_placeholder {
            parser.string()?
        } else {
            String::new()
        };
        Ok(ReplyMarkup {
            r#type,
            is_personal,
            force_reply,
            is_persistent,
            need_resize_keyboard,
            is_one_time_keyboard,
            keyboard,
            placeholder,
            inline_keyboard,
        })
    }
}

fn store_button_rows(out: &mut Vec<u8>, rows: &[Vec<KeyboardButton>]) {
    store_vec_header(out, rows.len());
    for row in rows {
        store_vec_header(out, row.len());
        for button in row {
            button.store(out);
        }
    }
}

fn parse_button_rows(parser: &mut Parser) -> Result<Vec<Vec<KeyboardButton>>, &'static str> {
    let rows_count = parser.i32_()?;
    if rows_count < 0 || rows_count as usize > parser.left_len() {
        return Err("Wrong vector length");
    }
    let mut rows = Vec::with_capacity(rows_count as usize);
    for _ in 0..rows_count {
        let buttons_count = parser.i32_()?;
        if buttons_count < 0 || buttons_count as usize > parser.left_len() {
            return Err("Wrong vector length");
        }
        let mut buttons = Vec::with_capacity(buttons_count as usize);
        for _ in 0..buttons_count {
            buttons.push(KeyboardButton::parse_from(parser)?);
        }
        rows.push(buttons);
    }
    Ok(rows)
}

fn store_inline_button_rows(out: &mut Vec<u8>, rows: &[Vec<InlineKeyboardButton>]) {
    store_vec_header(out, rows.len());
    for row in rows {
        store_vec_header(out, row.len());
        for button in row {
            button.store(out);
        }
    }
}

fn parse_inline_button_rows(
    parser: &mut Parser,
) -> Result<Vec<Vec<InlineKeyboardButton>>, &'static str> {
    let rows_count = parser.i32_()?;
    if rows_count < 0 || rows_count as usize > parser.left_len() {
        return Err("Wrong vector length");
    }
    let mut rows = Vec::with_capacity(rows_count as usize);
    for _ in 0..rows_count {
        let buttons_count = parser.i32_()?;
        if buttons_count < 0 || buttons_count as usize > parser.left_len() {
            return Err("Wrong vector length");
        }
        let mut buttons = Vec::with_capacity(buttons_count as usize);
        for _ in 0..buttons_count {
            buttons.push(InlineKeyboardButton::parse_from(parser)?);
        }
        rows.push(buttons);
    }
    Ok(rows)
}

// ---------------------------------------------------------------------------
// M.10.1b batch 2 — the Message/Dialog struct plane (MessagesManager.h
// 1118-1269 and 1308-1492), with the companions those structs pull in:
// SuggestedPost.h, MessageForwardInfo.h (LastForwardedMessageInfo),
// RepliedMessageInfo.h (MessageQuote), MessageInputReplyTo.h,
// InputMessageText.h, DraftMessage.h (+ the DraftMessageContent subclasses
// from DraftMessage.cpp:29-120), FileUploadId.h, and the
// logevent/LogEventHelper.h LogEventIdWithGeneration.
//
// Field order follows the C++ structs; the Rust structs keep the C++
// field-initializer defaults via manual `Default` impls (the C++
// `Message() = default` / `Dialog() = default`). The C++ nodes are
// move-only and identity-typed (`Message` inherits ListNode; copies are
// deleted), so neither struct derives Clone/PartialEq — only Debug +
// Default. Documented stand-ins:
// - `MessageReactions` (above), `ChatTheme` (needs the Wave-3
//   StarGift/ThemeSettings plane), `OrderedMessages`/`MessageLruList`
//   (intrusive C++ node containers; the receive-path slices pick the Rust
//   shape), `SendQueryRef` (the C++ `NetQueryRef` — [blocked: net]).
// - the `WaitFree*`/`Flat*` containers map to std `HashMap`/`HashSet` for
//   now (the M.4/M.5 convention).
// ---------------------------------------------------------------------------

/// `td::FileUploadId` (files/FileUploadId.h) — the FileManager-assigned
/// (file, internal upload id) pair. `get_file_upload_ids` (the
/// FileManager registry static) lands with the Files slices.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct FileUploadId {
    file_id: FileId,
    internal_upload_id: i64,
}

impl FileUploadId {
    pub fn new(file_id: FileId, internal_upload_id: i64) -> FileUploadId {
        FileUploadId {
            file_id,
            internal_upload_id,
        }
    }

    /// `FileUploadId::is_valid` (FileUploadId.h:26-28).
    pub fn is_valid(self) -> bool {
        self.file_id.is_valid()
    }

    /// `FileUploadId::get_file_id` (FileUploadId.h:31-33).
    pub fn get_file_id(self) -> FileId {
        self.file_id
    }

    /// `FileUploadId::get_internal_upload_id` (FileUploadId.h:35-37).
    pub fn get_internal_upload_id(self) -> i64 {
        self.internal_upload_id
    }
}

/// `td::LogEventIdWithGeneration` (logevent/LogEventHelper.h:20-23).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LogEventIdWithGeneration {
    pub log_event_id: u64,
    pub generation: u64,
}

/// `td::MessageQuote` (MessageQuote.h) — the text quote a reply points at.
/// The wire/td_api constructors and `search_quote` land with their slices;
/// the ctor keeps `max(0, position)` and the `remove_unallowed_quote_entities`
/// call for them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageQuote {
    pub text: FormattedText,
    pub position: i32,
    pub is_manual: bool,
}

impl MessageQuote {
    /// `MessageQuote::is_empty` (MessageQuote.h).
    pub fn is_empty(&self) -> bool {
        self.text.text.is_empty()
    }

    /// `MessageQuote::store` (MessageQuote.hpp:16-33).
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_text = !self.text.text.is_empty();
        let has_position = self.position != 0;
        store_flags(out, [has_text, has_position, self.is_manual]);
        if has_text {
            self.text.store(out);
        }
        if has_position {
            store_i32(out, self.position);
        }
    }

    /// `MessageQuote::parse` (MessageQuote.hpp:35-56) — the text is passed
    /// through `remove_unallowed_quote_entities` like the C++.
    pub fn parse_from(parser: &mut Parser) -> Result<MessageQuote, &'static str> {
        let [has_text, has_position, is_manual] = parser.parse_flags::<3>()?;
        let mut text = FormattedText::default();
        if has_text {
            text = FormattedText::parse(parser)?;
            crate::message_entity::remove_unallowed_quote_entities(&mut text);
        }
        let position = if has_position { parser.i32_()? } else { 0 };
        Ok(MessageQuote {
            text,
            position,
            is_manual,
        })
    }
}

/// `td::LastForwardedMessageInfo` (MessageForwardInfo.h:26-73) — where the
/// latest copy of a forwarded message comes from.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LastForwardedMessageInfo {
    pub dialog_id: DialogId,
    pub message_id: MessageId,
    pub sender_dialog_id: DialogId,
    pub sender_name: String,
    pub date: i32,
    pub is_outgoing: bool,
}

impl LastForwardedMessageInfo {
    pub fn new(
        dialog_id: DialogId,
        message_id: MessageId,
        sender_dialog_id: DialogId,
        sender_name: String,
        date: i32,
        is_outgoing: bool,
    ) -> LastForwardedMessageInfo {
        LastForwardedMessageInfo {
            dialog_id,
            message_id,
            sender_dialog_id,
            sender_name,
            date,
            is_outgoing,
        }
    }

    /// `LastForwardedMessageInfo::has_sender_name` (MessageForwardInfo.h).
    pub fn has_sender_name(&self) -> bool {
        !self.sender_name.is_empty()
    }

    pub fn get_dialog_id(self) -> DialogId {
        self.dialog_id
    }

    /// `LastForwardedMessageInfo::get_message_full_id` (MessageForwardInfo.h).
    pub fn get_message_full_id(self) -> MessageFullId {
        MessageFullId::new(self.dialog_id, self.message_id)
    }

    /// `LastForwardedMessageInfo::is_empty` (MessageForwardInfo.cpp:21-23).
    pub fn is_empty(&self) -> bool {
        *self == LastForwardedMessageInfo::default()
    }

    /// `LastForwardedMessageInfo::validate` (MessageForwardInfo.cpp:25-33).
    pub fn validate(&mut self) {
        if self.dialog_id.is_valid() != self.message_id.is_valid()
            || (self.sender_dialog_id != DialogId::default() && !self.sender_dialog_id.is_valid())
            || ((self.sender_dialog_id != DialogId::default() || !self.sender_name.is_empty())
                && self.date <= 0)
        {
            *self = LastForwardedMessageInfo::default();
        }
    }

    /// `LastForwardedMessageInfo::store` (MessageForwardInfo.hpp:16-37).
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_sender_dialog_id = self.sender_dialog_id.is_valid();
        let has_sender_name = !self.sender_name.is_empty();
        let has_date = self.date > 0;
        store_flags(
            out,
            [
                has_sender_dialog_id,
                has_sender_name,
                has_date,
                self.is_outgoing,
            ],
        );
        self.dialog_id.store(out);
        self.message_id.store(out);
        if has_sender_dialog_id {
            self.sender_dialog_id.store(out);
        }
        if has_sender_name {
            store_string(out, &self.sender_name);
        }
        if has_date {
            store_i32(out, self.date);
        }
    }

    /// `LastForwardedMessageInfo::parse` (MessageForwardInfo.hpp:39-62).
    pub fn parse_from(parser: &mut Parser) -> Result<LastForwardedMessageInfo, &'static str> {
        let [has_sender_dialog_id, has_sender_name, has_date, is_outgoing] =
            parser.parse_flags::<4>()?;
        let dialog_id = DialogId::parse_from(parser)?;
        let message_id = MessageId::parse_from(parser)?;
        let sender_dialog_id = if has_sender_dialog_id {
            DialogId::parse_from(parser)?
        } else {
            DialogId::default()
        };
        let sender_name = if has_sender_name {
            parser.string()?
        } else {
            String::new()
        };
        let date = if has_date { parser.i32_()? } else { 0 };
        let mut result = LastForwardedMessageInfo {
            dialog_id,
            message_id,
            sender_dialog_id,
            sender_name,
            date,
            is_outgoing,
        };
        result.validate();
        Ok(result)
    }
}

/// `td::MessageForwardInfo` (MessageForwardInfo.h:76-117).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageForwardInfo {
    pub origin: MessageOrigin,
    pub date: i32,
    pub last_message_info: LastForwardedMessageInfo,
    pub psa_type: String,
    pub is_imported: bool,
}

impl MessageForwardInfo {
    /// `MessageForwardInfo::get_last_dialog_id` (MessageForwardInfo.h).
    pub fn get_last_dialog_id(&self) -> DialogId {
        self.last_message_info.dialog_id
    }

    /// `MessageForwardInfo::get_last_message_full_id`
    /// (MessageForwardInfo.h).
    pub fn get_last_message_full_id(&self) -> MessageFullId {
        MessageFullId::new(
            self.last_message_info.dialog_id,
            self.last_message_info.message_id,
        )
    }

    /// `MessageForwardInfo::store` (MessageForwardInfo.hpp:64-81).
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_last_message_info = !self.last_message_info.is_empty();
        let has_psa_type = !self.psa_type.is_empty();
        store_flags(out, [self.is_imported, has_last_message_info, has_psa_type]);
        self.origin.store(out);
        store_i32(out, self.date);
        if has_last_message_info {
            self.last_message_info.store(out);
        }
        if has_psa_type {
            store_string(out, &self.psa_type);
        }
    }

    /// `MessageForwardInfo::parse` (MessageForwardInfo.hpp:83-99).
    pub fn parse_from(parser: &mut Parser) -> Result<MessageForwardInfo, &'static str> {
        let [is_imported, has_last_message_info, has_psa_type] = parser.parse_flags::<3>()?;
        let origin = MessageOrigin::parse_from(parser)?;
        let date = parser.i32_()?;
        let last_message_info = if has_last_message_info {
            LastForwardedMessageInfo::parse_from(parser)?
        } else {
            LastForwardedMessageInfo::default()
        };
        let psa_type = if has_psa_type {
            parser.string()?
        } else {
            String::new()
        };
        Ok(MessageForwardInfo {
            origin,
            date,
            last_message_info,
            psa_type,
            is_imported,
        })
    }
}

/// `td::RepliedMessageInfo` (RepliedMessageInfo.h:23-50) — the reply header
/// of a message; `origin_date`/`origin`/`content` are filled only for
/// replies to messages from other chats. The wire/td_api constructors and
/// the `Td`-taking methods land with their slices.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RepliedMessageInfo {
    pub message_id: MessageId,
    pub dialog_id: DialogId,
    pub origin_date: i32,
    pub origin: MessageOrigin,
    pub content: Option<MessageContent>,
    pub quote: MessageQuote,
    pub todo_item_id: i32,
    pub poll_option_id: String,
}

impl RepliedMessageInfo {
    /// `RepliedMessageInfo::legacy` (RepliedMessageInfo.h:57-62).
    pub fn legacy(
        reply_to_message_id: MessageId,
        reply_in_dialog_id: DialogId,
    ) -> RepliedMessageInfo {
        RepliedMessageInfo {
            message_id: reply_to_message_id,
            dialog_id: reply_in_dialog_id,
            ..RepliedMessageInfo::default()
        }
    }

    /// `RepliedMessageInfo::is_empty` (RepliedMessageInfo.h:67-69).
    pub fn is_empty(&self) -> bool {
        self.message_id == MessageId::default() && self.origin.is_empty()
    }

    /// `RepliedMessageInfo::is_external` (RepliedMessageInfo.h:71-73).
    pub fn is_external(&self) -> bool {
        self.origin_date != 0
    }

    /// `RepliedMessageInfo::get_same_chat_reply_to_message_id`
    /// (RepliedMessageInfo.cpp:400-408).
    pub fn get_same_chat_reply_to_message_id(&self, ignore_external: bool) -> MessageId {
        if self.message_id == MessageId::default() {
            return MessageId::default();
        }
        if !ignore_external && !self.origin.is_empty() {
            return MessageId::default();
        }
        if self.dialog_id == DialogId::default() {
            self.message_id
        } else {
            MessageId::default()
        }
    }

    /// `RepliedMessageInfo::get_reply_message_full_id`
    /// (RepliedMessageInfo.cpp:410-418).
    pub fn get_reply_message_full_id(
        &self,
        owner_dialog_id: DialogId,
        ignore_external: bool,
    ) -> MessageFullId {
        if self.message_id == MessageId::default() {
            return MessageFullId::default();
        }
        if ignore_external && !self.origin.is_empty() {
            return MessageFullId::default();
        }
        MessageFullId {
            dialog_id: if self.dialog_id.is_valid() {
                self.dialog_id
            } else {
                owner_dialog_id
            },
            message_id: self.message_id,
        }
    }

    /// `RepliedMessageInfo::store` (RepliedMessageInfo.hpp:16-59) — the
    /// embedded replied-to content rides the M.9 store seam.
    pub fn store(
        &self,
        ctx: &dyn crate::message_content::MessageContentStorage,
        out: &mut Vec<u8>,
    ) {
        let has_message_id = self.message_id.is_valid() || self.message_id.is_valid_scheduled();
        let has_dialog_id = self.dialog_id.is_valid();
        let has_origin_date = self.origin_date != 0;
        let has_origin = !self.origin.is_empty();
        let has_content = self.content.is_some();
        let has_quote = !self.quote.is_empty();
        let has_todo_item_id = self.todo_item_id != 0;
        let has_poll_option_id = !self.poll_option_id.is_empty();
        store_flags(
            out,
            [
                has_message_id,
                has_dialog_id,
                has_origin_date,
                has_origin,
                false,
                false,
                has_content,
                false,
                has_quote,
                has_todo_item_id,
                has_poll_option_id,
            ],
        );
        if has_message_id {
            self.message_id.store(out);
        }
        if has_dialog_id {
            self.dialog_id.store(out);
        }
        if has_origin_date {
            store_i32(out, self.origin_date);
        }
        if has_origin {
            self.origin.store(out);
        }
        if has_content {
            crate::message_content::store_message_content(self.content.as_ref().unwrap(), ctx, out);
        }
        if has_quote {
            self.quote.store(out);
        }
        if has_todo_item_id {
            store_i32(out, self.todo_item_id);
        }
        if has_poll_option_id {
            store_string(out, &self.poll_option_id);
        }
    }

    /// `RepliedMessageInfo::parse` (RepliedMessageInfo.hpp:61-127) — the
    /// legacy quote/position fields collapse into a `MessageQuote`.
    pub fn parse_from(
        parser: &mut Parser,
        ctx: &mut dyn crate::message_content::MessageContentStorage,
    ) -> Result<RepliedMessageInfo, &'static str> {
        let [
            has_message_id,
            has_dialog_id,
            has_origin_date,
            has_origin,
            has_quote_legacy,
            is_quote_manual_legacy,
            has_content,
            has_quote_position_legacy,
            has_quote,
            has_todo_item_id,
            has_poll_option_id,
        ] = parser.parse_flags::<11>()?;
        let mut result = RepliedMessageInfo::default();
        if has_message_id {
            result.message_id = MessageId::parse_from(parser)?;
        }
        if has_dialog_id {
            result.dialog_id = DialogId::parse_from(parser)?;
        }
        if has_origin_date {
            result.origin_date = parser.i32_()?;
        }
        if has_origin {
            result.origin = MessageOrigin::parse_from(parser)?;
        }
        let quote_legacy = if has_quote_legacy {
            Some(FormattedText::parse(parser)?)
        } else {
            None
        };
        if has_content {
            result.content = Some(crate::message_content::parse_message_content(parser, ctx)?);
        }
        let quote_position_legacy = if has_quote_position_legacy {
            parser.i32_()?
        } else {
            0
        };
        if has_quote {
            result.quote = MessageQuote::parse_from(parser)?;
        } else if let Some(text) = quote_legacy {
            result.quote = MessageQuote {
                text,
                position: quote_position_legacy,
                is_manual: is_quote_manual_legacy,
            };
        }
        if has_todo_item_id {
            result.todo_item_id = parser.i32_()?;
        }
        if has_poll_option_id {
            result.poll_option_id = parser.string()?;
        }
        Ok(result)
    }
}

/// `td::MessageInputReplyTo` (MessageInputReplyTo.h:19-29) — the reply
/// description sent with a message. `debug_source` defaults to
/// `"unknown"` like the C++ `const char *`.
#[derive(Clone, Debug, PartialEq)]
pub struct MessageInputReplyTo {
    pub message_id: MessageId,
    pub ephemeral_message_id: EphemeralMessageId,
    pub dialog_id: DialogId,
    pub quote: MessageQuote,
    pub todo_item_id: i32,
    pub poll_option_id: String,
    pub debug_source: &'static str,
    pub story_full_id: StoryFullId,
}

impl Default for MessageInputReplyTo {
    fn default() -> MessageInputReplyTo {
        MessageInputReplyTo {
            message_id: MessageId::default(),
            ephemeral_message_id: EphemeralMessageId::default(),
            dialog_id: DialogId::default(),
            quote: MessageQuote::default(),
            todo_item_id: 0,
            poll_option_id: String::new(),
            debug_source: "unknown",
            story_full_id: StoryFullId::default(),
        }
    }
}

impl MessageInputReplyTo {
    /// `MessageInputReplyTo::regular` (MessageInputReplyTo.cpp:14-20).
    pub fn regular(message_id: MessageId) -> MessageInputReplyTo {
        MessageInputReplyTo {
            message_id,
            ..MessageInputReplyTo::default()
        }
    }

    /// `MessageInputReplyTo::is_empty` (MessageInputReplyTo.h:72-75).
    pub fn is_empty(&self) -> bool {
        !self.message_id.is_valid()
            && !self.message_id.is_valid_scheduled()
            && !self.ephemeral_message_id.is_valid()
            && !self.story_full_id.is_valid()
    }

    /// `MessageInputReplyTo::is_valid` (MessageInputReplyTo.h:77-79).
    pub fn is_valid(&self) -> bool {
        !self.is_empty()
    }

    /// `MessageInputReplyTo::get_same_chat_reply_to_message_id`
    /// (MessageInputReplyTo.cpp:12-15).
    pub fn get_same_chat_reply_to_message_id(&self) -> MessageId {
        if self.dialog_id == DialogId::default()
            && (self.message_id.is_valid() || self.message_id.is_valid_scheduled())
        {
            self.message_id
        } else {
            MessageId::default()
        }
    }

    /// `MessageInputReplyTo::store` (MessageInputReplyTo.hpp:16-62).
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_message_id = self.message_id.is_valid();
        let has_story_full_id = self.story_full_id.is_valid();
        let has_dialog_id = self.dialog_id.is_valid();
        let has_quote = !self.quote.is_empty();
        let has_todo_item_id = self.todo_item_id != 0;
        let has_poll_option_id = !self.poll_option_id.is_empty();
        let has_ephemeral_message_id = self.ephemeral_message_id.is_valid();
        store_flags(
            out,
            [
                has_message_id,
                has_story_full_id,
                false,
                has_dialog_id,
                false,
                has_quote,
                has_todo_item_id,
                has_poll_option_id,
                has_ephemeral_message_id,
            ],
        );
        if has_message_id {
            self.message_id.store(out);
        }
        if has_story_full_id {
            self.story_full_id.store(out);
        }
        if has_dialog_id {
            self.dialog_id.store(out);
        }
        if has_quote {
            self.quote.store(out);
        }
        if has_todo_item_id {
            store_i32(out, self.todo_item_id);
        }
        if has_poll_option_id {
            store_string(out, &self.poll_option_id);
        }
        if has_ephemeral_message_id {
            self.ephemeral_message_id.store(out);
        }
    }

    /// `MessageInputReplyTo::parse` (MessageInputReplyTo.hpp:64-122) — the
    /// legacy quote fields collapse into a `MessageQuote` like the C++.
    pub fn parse_from(parser: &mut Parser) -> Result<MessageInputReplyTo, &'static str> {
        let [
            has_message_id,
            has_story_full_id,
            has_quote_legacy,
            has_dialog_id,
            has_quote_position_legacy,
            has_quote,
            has_todo_item_id,
            has_poll_option_id,
            has_ephemeral_message_id,
        ] = parser.parse_flags::<9>()?;
        let mut result = MessageInputReplyTo::default();
        if has_message_id {
            result.message_id = MessageId::parse_from(parser)?;
            result.debug_source = "binlog";
        }
        if has_story_full_id {
            result.story_full_id = StoryFullId::parse_from(parser)?;
        }
        let quote_legacy = if has_quote_legacy {
            Some(FormattedText::parse(parser)?)
        } else {
            None
        };
        if has_dialog_id {
            result.dialog_id = DialogId::parse_from(parser)?;
        }
        let quote_position_legacy = if has_quote_position_legacy {
            parser.i32_()?
        } else {
            0
        };
        if has_quote {
            result.quote = MessageQuote::parse_from(parser)?;
        } else if let Some(text) = quote_legacy {
            result.quote = MessageQuote {
                text,
                position: quote_position_legacy,
                is_manual: false,
            };
        }
        if has_todo_item_id {
            result.todo_item_id = parser.i32_()?;
        }
        if has_poll_option_id {
            result.poll_option_id = parser.string()?;
        }
        if has_ephemeral_message_id {
            result.ephemeral_message_id = EphemeralMessageId::parse_from(parser)?;
        }
        Ok(result)
    }
}

/// `td::InputMessageText` (InputMessageText.h:24-33).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct InputMessageText {
    pub text: FormattedText,
    pub web_page_url: String,
    pub disable_web_page_preview: bool,
    pub force_small_media: bool,
    pub force_large_media: bool,
    pub show_above_text: bool,
    pub clear_draft: bool,
}

impl InputMessageText {
    /// `InputMessageText::is_empty` (InputMessageText.h:47-50).
    pub fn is_empty(&self) -> bool {
        self.text.text.is_empty() && self.web_page_url.is_empty()
    }

    /// `store(const InputMessageText &, ...)` (InputMessageText.hpp:16-37).
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_web_page_url = !self.web_page_url.is_empty();
        let has_empty_text = self.text.text.is_empty();
        store_flags(
            out,
            [
                self.disable_web_page_preview,
                self.clear_draft,
                self.force_small_media,
                self.force_large_media,
                has_web_page_url,
                has_empty_text,
            ],
        );
        if !has_empty_text {
            self.text.store(out);
        }
        if has_web_page_url {
            store_string(out, &self.web_page_url);
        }
    }

    /// `parse(InputMessageText &, ...)` (InputMessageText.hpp:39-59).
    pub fn parse_from(parser: &mut Parser) -> Result<InputMessageText, &'static str> {
        let [
            disable_web_page_preview,
            clear_draft,
            force_small_media,
            force_large_media,
            has_web_page_url,
            has_empty_text,
        ] = parser.parse_flags::<6>()?;
        let text = if !has_empty_text {
            FormattedText::parse(parser)?
        } else {
            FormattedText::default()
        };
        let web_page_url = if has_web_page_url {
            parser.string()?
        } else {
            String::new()
        };
        Ok(InputMessageText {
            text,
            web_page_url,
            disable_web_page_preview,
            force_small_media,
            force_large_media,
            show_above_text: false,
            clear_draft,
        })
    }
}

/// `td::SuggestedPost` (SuggestedPost.h) — the model half; the wire/td_api
/// constructors live in SuggestedPost.cpp and land with the consuming
/// slices. `price` re-points at the M.9-line `message_content`
/// `SuggestedPostPrice` (batch 1 deliberately left it undefined here).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SuggestedPost {
    pub price: SuggestedPostPrice,
    pub schedule_date: i32,
    pub is_accepted: bool,
    pub is_rejected: bool,
}

impl SuggestedPost {
    /// `SuggestedPost::is_pending` (SuggestedPost.h).
    pub fn is_pending(&self) -> bool {
        !self.is_accepted && !self.is_rejected
    }

    /// `SuggestedPost::get_schedule_date` (SuggestedPost.h).
    pub fn get_schedule_date(&self) -> i32 {
        self.schedule_date
    }

    /// `SuggestedPost::store` (SuggestedPost.hpp:14-31).
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_price = !self.price.is_empty();
        let has_schedule_date = self.schedule_date != 0;
        store_flags(
            out,
            [
                self.is_accepted,
                self.is_rejected,
                has_price,
                has_schedule_date,
            ],
        );
        if has_price {
            store_suggested_post_price(out, &self.price);
        }
        if has_schedule_date {
            store_i32(out, self.schedule_date);
        }
    }

    /// `SuggestedPost::parse` (SuggestedPost.hpp:33-48).
    pub fn parse_from(parser: &mut Parser) -> Result<SuggestedPost, &'static str> {
        let [is_accepted, is_rejected, has_price, has_schedule_date] = parser.parse_flags::<4>()?;
        let price = if has_price {
            parse_suggested_post_price(parser)?
        } else {
            SuggestedPostPrice::default()
        };
        let schedule_date = if has_schedule_date { parser.i32_()? } else { 0 };
        Ok(SuggestedPost {
            price,
            schedule_date,
            is_accepted,
            is_rejected,
        })
    }
}

/// `SuggestedPostPrice::store` (SuggestedPostPrice.hpp:14-26) — lives here
/// (not in `message_content`) because the M.9 line owns the type but the
/// store/parse plane arrived with M.10.2; move at the next M.9 touch if
/// the split bothers you.
pub fn store_suggested_post_price(out: &mut Vec<u8>, price: &SuggestedPostPrice) {
    let has_amount = price.amount != 0;
    store_flags(out, [has_amount]);
    store_i32(
        out,
        match price.r#type {
            crate::message_content::SuggestedPostPriceType::None => 0,
            crate::message_content::SuggestedPostPriceType::Star => 1,
            crate::message_content::SuggestedPostPriceType::Ton => 2,
        },
    );
    if has_amount {
        store_i64(out, price.amount);
    }
}

/// `SuggestedPostPrice::parse` (SuggestedPostPrice.hpp:28-38).
pub fn parse_suggested_post_price(parser: &mut Parser) -> Result<SuggestedPostPrice, &'static str> {
    let [has_amount] = parser.parse_flags::<1>()?;
    let price_type = parser.i32_()?;
    let amount = if has_amount { parser.i64_()? } else { 0 };
    let r#type = match price_type {
        0 => crate::message_content::SuggestedPostPriceType::None,
        1 => crate::message_content::SuggestedPostPriceType::Star,
        2 => crate::message_content::SuggestedPostPriceType::Ton,
        _ => return Err("Wrong suggested post price type"),
    };
    Ok(SuggestedPostPrice { r#type, amount })
}

/// `td::DraftMessageContentType` (DraftMessage.h:33).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
pub enum DraftMessageContentType {
    VideoNote = 0,
    VoiceNote = 1,
}

/// `td::DraftMessageContentVideoNote` (DraftMessage.cpp:29-48).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DraftMessageContentVideoNote {
    pub path: String,
    pub duration: i32,
    pub length: i32,
    pub ttl: MessageSelfDestructType,
}

/// `td::DraftMessageContentVoiceNote` (DraftMessage.cpp:104-123).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DraftMessageContentVoiceNote {
    pub path: String,
    pub duration: i32,
    pub waveform: String,
    pub ttl: MessageSelfDestructType,
}

/// The `DraftMessageContent` C++ class hierarchy (DraftMessage.h:35-48)
/// flattened into an enum of structs; the td_api objects land with the
/// drafts slice.
#[derive(Clone, Debug, PartialEq)]
pub enum DraftMessageContent {
    VideoNote(DraftMessageContentVideoNote),
    VoiceNote(DraftMessageContentVoiceNote),
}

impl DraftMessageContent {
    /// The per-class `get_type` overrides (DraftMessage.cpp:44-48/119-123).
    pub fn get_type(&self) -> DraftMessageContentType {
        match self {
            DraftMessageContent::VideoNote(_) => DraftMessageContentType::VideoNote,
            DraftMessageContent::VoiceNote(_) => DraftMessageContentType::VoiceNote,
        }
    }

    /// The per-class `store` overrides (DraftMessage.cpp:40-48/115-123 and
    /// the `store(const DraftMessageContent *, ...)` switch at 172-193) —
    /// the discriminant then the flags/payload, i.e. exactly
    /// `store_draft_message_content` after the type word.
    pub fn store(&self, out: &mut Vec<u8>) {
        store_i32(out, self.get_type() as i32);
        match self {
            DraftMessageContent::VideoNote(video_note) => {
                let has_path = !video_note.path.is_empty();
                let has_duration = video_note.duration != 0;
                let has_length = video_note.length != 0;
                let has_ttl = video_note.ttl.is_valid();
                store_flags(out, [has_path, has_duration, has_length, has_ttl]);
                if has_path {
                    store_string(out, &video_note.path);
                }
                if has_duration {
                    store_i32(out, video_note.duration);
                }
                if has_length {
                    store_i32(out, video_note.length);
                }
                if has_ttl {
                    video_note.ttl.store(out);
                }
            }
            DraftMessageContent::VoiceNote(voice_note) => {
                let has_path = !voice_note.path.is_empty();
                let has_duration = voice_note.duration != 0;
                let has_waveform = !voice_note.waveform.is_empty();
                let has_ttl = voice_note.ttl.is_valid();
                store_flags(out, [has_path, has_duration, has_waveform, has_ttl]);
                if has_path {
                    store_string(out, &voice_note.path);
                }
                if has_duration {
                    store_i32(out, voice_note.duration);
                }
                if has_waveform {
                    store_string(out, &voice_note.waveform);
                }
                if has_ttl {
                    voice_note.ttl.store(out);
                }
            }
        }
    }

    /// `parse_draft_message_content` (DraftMessage.cpp:208-228).
    pub fn parse_from(parser: &mut Parser) -> Result<DraftMessageContent, &'static str> {
        let content_type = parser.i32_()?;
        match content_type {
            0 => {
                let [has_path, has_duration, has_length, has_ttl] = parser.parse_flags::<4>()?;
                Ok(DraftMessageContent::VideoNote(
                    DraftMessageContentVideoNote {
                        path: if has_path {
                            parser.string()?
                        } else {
                            String::new()
                        },
                        duration: if has_duration { parser.i32_()? } else { 0 },
                        length: if has_length { parser.i32_()? } else { 0 },
                        ttl: if has_ttl {
                            MessageSelfDestructType::parse_from(parser)?
                        } else {
                            MessageSelfDestructType::default()
                        },
                    },
                ))
            }
            1 => {
                let [has_path, has_duration, has_waveform, has_ttl] = parser.parse_flags::<4>()?;
                Ok(DraftMessageContent::VoiceNote(
                    DraftMessageContentVoiceNote {
                        path: if has_path {
                            parser.string()?
                        } else {
                            String::new()
                        },
                        duration: if has_duration { parser.i32_()? } else { 0 },
                        waveform: if has_waveform {
                            parser.string()?
                        } else {
                            String::new()
                        },
                        ttl: if has_ttl {
                            MessageSelfDestructType::parse_from(parser)?
                        } else {
                            MessageSelfDestructType::default()
                        },
                    },
                ))
            }
            _ => Err("Wrong draft content type"),
        }
    }
}

impl MessageSelfDestructType {
    /// The `ParserT` overload of `MessageSelfDestructType::parse`
    /// (MessageSelfDestructType.hpp) — the nested store/parse planes read
    /// it inline.
    pub fn parse_from(parser: &mut Parser) -> Result<MessageSelfDestructType, &'static str> {
        Ok(MessageSelfDestructType {
            ttl: parser.i32_()?,
        })
    }
}

/// `td::DraftMessage` (DraftMessage.h:50-60) — `rich_message_content` must
/// be `MessageRichText` once RichText lands (Wave-2); `local_content` is
/// the not-yet-uploaded draft media.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DraftMessage {
    pub date: i32,
    pub message_input_reply_to: MessageInputReplyTo,
    pub input_message_text: InputMessageText,
    pub rich_message_content: Option<MessageContent>,
    pub local_content: Option<DraftMessageContent>,
    pub message_effect_id: MessageEffectId,
    pub suggested_post: Option<SuggestedPost>,
}

impl DraftMessage {
    /// `DraftMessage::get_date` (DraftMessage.h:67-69).
    pub fn get_date(&self) -> i32 {
        self.date
    }

    /// `DraftMessage::is_local` (DraftMessage.h:71-73).
    pub fn is_local(&self) -> bool {
        self.local_content.is_some()
    }

    /// `DraftMessage::store` (DraftMessage.hpp:24-60). The rich-message
    /// arm stores through the M.9 content seam; the RichMessage legacy
    /// payload is a Wave-2 WebPageBlock block and stores as absent.
    pub fn store(
        &self,
        ctx: &dyn crate::message_content::MessageContentStorage,
        out: &mut Vec<u8>,
    ) {
        let has_input_message_text = !self.input_message_text.is_empty();
        let has_message_input_reply_to = !self.message_input_reply_to.is_empty();
        let has_local_content = self.local_content.is_some();
        let has_message_effect_id = self.message_effect_id.is_valid();
        let has_suggested_post = self.suggested_post.is_some();
        let is_rich = self.rich_message_content.is_some();
        store_flags(
            out,
            [
                has_input_message_text,
                has_message_input_reply_to,
                has_local_content,
                has_message_effect_id,
                has_suggested_post,
                false,
                is_rich,
            ],
        );
        store_i32(out, self.date);
        if has_input_message_text {
            self.input_message_text.store(out);
        }
        if has_message_input_reply_to {
            self.message_input_reply_to.store(out);
        }
        if has_local_content {
            self.local_content.as_ref().unwrap().store(out);
        }
        if has_message_effect_id {
            self.message_effect_id.store(out);
        }
        if has_suggested_post {
            self.suggested_post.as_ref().unwrap().store(out);
        }
        if is_rich {
            crate::message_content::store_message_content(
                self.rich_message_content.as_ref().unwrap(),
                ctx,
                out,
            );
        }
    }

    /// `DraftMessage::parse` (DraftMessage.hpp:62-121) — the pre-
    /// `SupportRepliesInOtherChats` (51) legacy layout stores the reply
    /// message id inline; the legacy RichMessage payload arm is a Wave-2
    /// block and errors like a truncated blob.
    pub fn parse_from(
        parser: &mut Parser,
        ctx: &mut dyn crate::message_content::MessageContentStorage,
    ) -> Result<DraftMessage, &'static str> {
        let mut result = DraftMessage::default();
        let has_input_message_text;
        let mut has_message_input_reply_to = false;
        let mut has_local_content = false;
        let mut has_message_effect_id = false;
        let mut has_suggested_post = false;
        let mut is_rich_legacy = false;
        let mut is_rich = false;
        let has_legacy_reply_to_message_id;
        if parser.version() >= crate::store_parse::version::SUPPORT_REPLIES_IN_OTHER_CHATS {
            has_legacy_reply_to_message_id = false;
            let [
                has_input_message_text_,
                has_message_input_reply_to_,
                has_local_content_,
                has_message_effect_id_,
                has_suggested_post_,
                is_rich_legacy_,
                is_rich_,
            ] = parser.parse_flags::<7>()?;
            has_input_message_text = has_input_message_text_;
            has_message_input_reply_to = has_message_input_reply_to_;
            has_local_content = has_local_content_;
            has_message_effect_id = has_message_effect_id_;
            has_suggested_post = has_suggested_post_;
            is_rich_legacy = is_rich_legacy_;
            is_rich = is_rich_;
        } else {
            has_legacy_reply_to_message_id = true;
            has_input_message_text = true;
        }
        result.date = parser.i32_()?;
        if has_legacy_reply_to_message_id {
            let legacy_reply_to_message_id = MessageId::parse_from(parser)?;
            result.message_input_reply_to =
                MessageInputReplyTo::regular(legacy_reply_to_message_id);
        }
        if has_input_message_text {
            result.input_message_text = InputMessageText::parse_from(parser)?;
        }
        if has_message_input_reply_to {
            result.message_input_reply_to = MessageInputReplyTo::parse_from(parser)?;

            let message_id = result
                .message_input_reply_to
                .get_same_chat_reply_to_message_id();
            if message_id.is_valid() && (message_id.is_yet_unsent() || message_id.is_local()) {
                result.message_input_reply_to = MessageInputReplyTo::default();
            }
        }
        if has_local_content {
            result.local_content = Some(DraftMessageContent::parse_from(parser)?);
        }
        if has_message_effect_id {
            result.message_effect_id = MessageEffectId::parse_from(parser)?;
        }
        if has_suggested_post {
            result.suggested_post = Some(SuggestedPost::parse_from(parser)?);
        }
        if is_rich_legacy {
            return Err("Legacy RichMessage draft payload is not supported yet");
        }
        if is_rich {
            result.rich_message_content =
                Some(crate::message_content::parse_message_content(parser, ctx)?);
        }
        Ok(result)
    }
}

/// The C++ `NetQueryRef send_query_ref` (MessagesManager.h:1256) — the
/// in-flight send query of a yet-unsent message. STAND-IN while net is
/// blocked; the net step replaces it with the real reference handle.
#[derive(Clone, Copy, Debug, Default)]
pub struct SendQueryRef {
    // [blocked: net]
}

/// `td::ChatTheme` — STAND-IN: needs the `StarGift` + `ThemeSettings`
/// plane (ChatTheme.h); the Wave-3 Gifts slice owns the real model. The
/// `Dialog` fields only need an opaque owned value meanwhile.
#[derive(Clone, Debug, Default)]
pub struct ChatTheme {
    // ported with the Wave-3 gifts/theme slice
}

/// The C++ `OrderedMessages` container (OrderedMessages.h) — an intrusive
/// tree of `Message *` nodes. STAND-IN until the receive-path slices pick
/// the Rust shape (the messages themselves live in
/// [`Dialog::messages`]).
#[derive(Clone, Copy, Debug, Default)]
pub struct OrderedMessages {
    // receiver-path slice
}

/// The C++ `ListNode message_lru_list` (MessagesManager.h:1470) — the
/// dialog's message LRU chain. STAND-IN for the same reason as
/// [`OrderedMessages`].
#[derive(Clone, Copy, Debug, Default)]
pub struct MessageLruList {
    // receiver-path slice
}

/// `td::MessagesManager::DialogScheduledMessages` (MessagesManager.h:
/// 1280-1287). No Clone — the scheduled `Message` nodes are identity-typed.
#[derive(Debug, Default)]
pub struct DialogScheduledMessages {
    pub scheduled_message_date: HashMap<ScheduledServerMessageId, i32>,
    pub last_assigned_scheduled_message_id: HashMap<i32, MessageId>,
    pub deleted_scheduled_server_message_ids: HashSet<ScheduledServerMessageId>,
    pub scheduled_messages: HashMap<MessageId, Box<Message>>,
}

/// `td::MessagesManager::Message` (MessagesManager.h:1118-1269) — the
/// ~95-field message node. Field order and field-initializer defaults
/// follow the C++ struct exactly (`max_reply_media_timestamp = -1`,
/// `max_own_media_timestamp = -2`, `debug_source = "null"`); the node is
/// identity-typed in C++ (copies deleted), so Rust gives it neither Clone
/// nor PartialEq. Fields marked "for send_message"/"for resend_message"
/// are the send-path scratch space (M.10.8-M.10.11).
#[derive(Debug)]
pub struct Message {
    pub message_id: MessageId,
    pub sender_user_id: UserId,
    pub sender_dialog_id: DialogId,
    pub receiver_user_id: UserId,
    pub ephemeral_message_id: EphemeralMessageId,
    pub saved_messages_topic_id: SavedMessagesTopicId,
    pub date: i32,
    pub edit_date: i32,
    pub send_date: i32,
    /// for yet unsent messages
    pub sending_id: i32,
    pub sender_boost_count: i32,
    pub paid_message_star_count: i64,
    pub chat_instance: i64,
    pub random_id: i64,
    pub forward_info: Option<MessageForwardInfo>,
    pub replied_message_info: RepliedMessageInfo,
    pub reply_to_story_full_id: StoryFullId,
    pub top_thread_message_id: MessageId,
    pub linked_top_thread_message_id: MessageId,
    pub local_thread_message_ids: Vec<MessageId>,
    /// for send_message
    pub initial_sender_dialog_id: DialogId,
    /// for send_message
    pub initial_top_thread_message_id: MessageId,
    /// for send_message
    pub input_reply_to: MessageInputReplyTo,
    /// for send_message
    pub reply_to_random_id: i64,
    /// for send_message
    pub send_callback_query_id: i64,
    /// for send_message
    pub send_emoji: String,
    /// for send_message
    pub new_video_start_timestamp: i32,
    /// for send_message
    pub file_upload_ids: Vec<FileUploadId>,
    /// for send_message
    pub thumbnail_file_upload_ids: Vec<FileUploadId>,
    pub via_bot_user_id: UserId,
    pub guest_bot_via_dialog_id: DialogId,
    pub via_business_bot_user_id: UserId,
    pub restriction_reasons: Vec<RestrictionReason>,
    pub author_signature: String,
    pub summary_from_language: String,
    pub sender_rank: String,
    pub is_channel_post: bool,
    pub is_topic_message: bool,
    pub is_outgoing: bool,
    pub is_failed_to_send: bool,
    pub disable_notification: bool,
    pub contains_mention: bool,
    pub contains_unread_mention: bool,
    pub hide_edit_date: bool,
    /// had non-inline reply markup?
    pub had_reply_markup: bool,
    pub had_forward_info: bool,
    /// must be shown only while tapped
    pub is_content_secret: bool,
    pub is_mention_notification_disabled: bool,
    pub is_from_scheduled: bool,
    pub is_from_offline: bool,
    pub is_pinned: bool,
    pub are_media_timestamp_entities_found: bool,
    pub noforwards: bool,
    pub invert_media: bool,
    pub disable_web_page_preview: bool,
    pub video_processing_pending: bool,
    pub reactions_are_possible: bool,
    pub is_paid_suggested_post_stars: bool,
    pub is_paid_suggested_post_ton: bool,
    /// for send_message
    pub has_explicit_sender: bool,
    /// for send_message
    pub is_copy: bool,
    /// for send_message
    pub from_background: bool,
    /// for send_message
    pub allow_paid: bool,
    /// for send_message
    pub update_stickersets_order: bool,
    /// for send_message
    pub clear_draft: bool,
    /// for send_message
    pub in_game_share: bool,
    /// for send_message
    pub initial_is_topic_message: bool,
    /// for send_message
    pub is_quick_reply_message: bool,
    /// for send_message
    pub send_anchor: bool,
    /// for resend_message
    pub hide_via_bot: bool,
    /// for resend_message
    pub is_bot_start_message: bool,
    /// for resend_message
    pub real_forward_from_dialog_id: DialogId,
    /// for resend_message
    pub real_forward_from_message_id: MessageId,
    pub notification_id: NotificationId,
    pub removed_notification_id: NotificationId,
    pub max_reply_media_timestamp: i32,
    /// to update replied messages on the first load
    pub max_own_media_timestamp: i32,
    pub view_count: i32,
    pub forward_count: i32,
    pub reply_info: MessageReplyInfo,
    pub reactions: Option<MessageReactions>,
    pub fact_check: Option<FactCheck>,
    pub suggested_post: Option<SuggestedPost>,
    pub thread_draft_message: Option<DraftMessage>,
    pub available_reactions_generation: u32,
    pub interaction_info_update_date: i32,
    pub history_generation: u32,
    pub legacy_layer: i32,
    pub send_error_code: i32,
    pub send_error_message: String,
    pub try_resend_at: f64,
    /// counted from message send date
    pub ttl_period: i32,
    /// counted from message content view date
    pub ttl: MessageSelfDestructType,
    /// only for TTL
    pub ttl_expires_at: f64,
    pub media_album_id: i64,
    pub effect_id: MessageEffectId,
    pub content: Option<MessageContent>,
    pub reply_markup: Option<Box<ReplyMarkup>>,
    pub ephemeral_message: Option<Box<Message>>,
    pub schedule_repeat_period: i32,
    pub edited_schedule_date: i32,
    pub edited_schedule_repeat_period: i32,
    pub edit_generation: u64,
    pub last_edit_pts: i32,
    pub debug_source: &'static str,
    pub last_access_date: i32,
    /// whether the message is known to the app
    pub is_update_sent: bool,
    pub send_message_log_event_id: u64,
    pub send_query_ref: SendQueryRef,
}

impl Default for Message {
    /// The C++ field initializers (MessagesManager.h:1119-1267).
    fn default() -> Message {
        Message {
            message_id: MessageId::default(),
            sender_user_id: UserId::default(),
            sender_dialog_id: DialogId::default(),
            receiver_user_id: UserId::default(),
            ephemeral_message_id: EphemeralMessageId::default(),
            saved_messages_topic_id: SavedMessagesTopicId::default(),
            date: 0,
            edit_date: 0,
            send_date: 0,
            sending_id: 0,
            sender_boost_count: 0,
            paid_message_star_count: 0,
            chat_instance: 0,
            random_id: 0,
            forward_info: None,
            replied_message_info: RepliedMessageInfo::default(),
            reply_to_story_full_id: StoryFullId::default(),
            top_thread_message_id: MessageId::default(),
            linked_top_thread_message_id: MessageId::default(),
            local_thread_message_ids: Vec::new(),
            initial_sender_dialog_id: DialogId::default(),
            initial_top_thread_message_id: MessageId::default(),
            input_reply_to: MessageInputReplyTo::default(),
            reply_to_random_id: 0,
            send_callback_query_id: 0,
            send_emoji: String::new(),
            new_video_start_timestamp: 0,
            file_upload_ids: Vec::new(),
            thumbnail_file_upload_ids: Vec::new(),
            via_bot_user_id: UserId::default(),
            guest_bot_via_dialog_id: DialogId::default(),
            via_business_bot_user_id: UserId::default(),
            restriction_reasons: Vec::new(),
            author_signature: String::new(),
            summary_from_language: String::new(),
            sender_rank: String::new(),
            is_channel_post: false,
            is_topic_message: false,
            is_outgoing: false,
            is_failed_to_send: false,
            disable_notification: false,
            contains_mention: false,
            contains_unread_mention: false,
            hide_edit_date: false,
            had_reply_markup: false,
            had_forward_info: false,
            is_content_secret: false,
            is_mention_notification_disabled: false,
            is_from_scheduled: false,
            is_from_offline: false,
            is_pinned: false,
            are_media_timestamp_entities_found: false,
            noforwards: false,
            invert_media: false,
            disable_web_page_preview: false,
            video_processing_pending: false,
            reactions_are_possible: false,
            is_paid_suggested_post_stars: false,
            is_paid_suggested_post_ton: false,
            has_explicit_sender: false,
            is_copy: false,
            from_background: false,
            allow_paid: false,
            update_stickersets_order: false,
            clear_draft: false,
            in_game_share: false,
            initial_is_topic_message: false,
            is_quick_reply_message: false,
            send_anchor: false,
            hide_via_bot: false,
            is_bot_start_message: false,
            real_forward_from_dialog_id: DialogId::default(),
            real_forward_from_message_id: MessageId::default(),
            notification_id: NotificationId::default(),
            removed_notification_id: NotificationId::default(),
            max_reply_media_timestamp: -1,
            max_own_media_timestamp: -2,
            view_count: 0,
            forward_count: 0,
            reply_info: MessageReplyInfo::default(),
            reactions: None,
            fact_check: None,
            suggested_post: None,
            thread_draft_message: None,
            available_reactions_generation: 0,
            interaction_info_update_date: 0,
            history_generation: 0,
            legacy_layer: 0,
            send_error_code: 0,
            send_error_message: String::new(),
            try_resend_at: 0.0,
            ttl_period: 0,
            ttl: MessageSelfDestructType::default(),
            ttl_expires_at: 0.0,
            media_album_id: 0,
            effect_id: MessageEffectId::default(),
            content: None,
            reply_markup: None,
            ephemeral_message: None,
            schedule_repeat_period: 0,
            edited_schedule_date: 0,
            edited_schedule_repeat_period: 0,
            edit_generation: 0,
            last_edit_pts: 0,
            debug_source: "null",
            last_access_date: 0,
            is_update_sent: false,
            send_message_log_event_id: 0,
            send_query_ref: SendQueryRef::default(),
        }
    }
}

/// `get_default_service_message_content_reactions_are_possible`
/// (MessageContentType.cpp:1144-1176) — the static default for
/// `Message::reactions_are_possible`; the classification family lives in
/// M.9's `message_content`, this entry arrives with the M.10 store plane
/// (move at the next M.9 touch).
pub fn get_default_service_message_content_reactions_are_possible(
    content_type: MessageContentType,
) -> bool {
    matches!(
        content_type,
        MessageContentType::BoostApply
            | MessageContentType::Call
            | MessageContentType::ChangeCommunity
            | MessageContentType::ChangeCreator
            | MessageContentType::ChatAddUsers
            | MessageContentType::ChatChangePhoto
            | MessageContentType::ChatChangeTitle
            | MessageContentType::ChatDeletePhoto
            | MessageContentType::ChatDeleteUser
            | MessageContentType::ChatJoinedByLink
            | MessageContentType::ChatJoinedViaCommunity
            | MessageContentType::ChatSetTheme
            | MessageContentType::ChatSetTtl
            | MessageContentType::ConferenceCall
            | MessageContentType::ContactRegistered
            | MessageContentType::GameScore
            | MessageContentType::GiftCode
            | MessageContentType::GiftPremium
            | MessageContentType::GiftStars
            | MessageContentType::GiveawayResults
            | MessageContentType::GroupCall
            | MessageContentType::InviteToGroupCall
            | MessageContentType::ManagedBotCreated
            | MessageContentType::NewCreatorPending
            | MessageContentType::NoForwardsRequest
            | MessageContentType::NoForwardsToggle
            | MessageContentType::PaidMessagesPrice
            | MessageContentType::PaidMessagesRefunded
            | MessageContentType::PaymentRefunded
            | MessageContentType::PaymentSuccessful
            | MessageContentType::PinMessage
            | MessageContentType::PollAppendAnswer
            | MessageContentType::PollDeleteAnswer
            | MessageContentType::PrizeStars
            | MessageContentType::ProximityAlertTriggered
            | MessageContentType::ScreenshotTaken
            | MessageContentType::SetBackground
            | MessageContentType::StarGift
            | MessageContentType::StarGiftPurchaseOffer
            | MessageContentType::StarGiftPurchaseOfferDeclined
            | MessageContentType::StarGiftUnique
            | MessageContentType::SuggestBirthday
            | MessageContentType::SuggestedPostApproval
            | MessageContentType::SuggestedPostRefund
            | MessageContentType::SuggestedPostSuccess
            | MessageContentType::SuggestProfilePhoto
            | MessageContentType::TodoAppendTasks
            | MessageContentType::TodoCompletions
            | MessageContentType::TopicCreate
            | MessageContentType::TopicEdit
            | MessageContentType::WebsiteConnected
            | MessageContentType::WebViewWriteAccessAllowed
            | MessageContentType::WriteAccessAllowed
            | MessageContentType::WriteAccessAllowedByRequest
    )
}

impl Message {
    /// `MessagesManager::Message::store` (MessagesManager.cpp:2308-2655).
    /// The `TimeCtx` carries the `store_time` clocks (LogEventHelper.h).
    pub fn store(
        &self,
        storage: &dyn crate::message_content::MessageContentStorage,
        time: &crate::user_manager::TimeCtx,
        out: &mut Vec<u8>,
    ) {
        let has_sender_user_id = self.sender_user_id.is_valid();
        let has_edit_date = self.edit_date > 0;
        let has_random_id = self.random_id != 0;
        let is_reply_to_random_id = self.reply_to_random_id != 0;
        let is_via_bot = self.via_bot_user_id.is_valid();
        let has_view_count = self.view_count > 0;
        let has_reply_markup = self.reply_markup.is_some();
        let has_ttl = self.ttl.is_valid();
        let has_author_signature = !self.author_signature.is_empty();
        let has_media_album_id = self.media_album_id != 0;
        let has_send_date = self.message_id.is_yet_unsent() && self.send_date != 0;
        let has_flags2 = true;
        let has_notification_id = self.notification_id.is_valid();
        let has_send_error_code = self.send_error_code != 0;
        let has_real_forward_from = self.real_forward_from_dialog_id.is_valid()
            && self.real_forward_from_message_id.is_valid();
        let has_legacy_layer = self.legacy_layer != 0;
        let has_restriction_reasons = !self.restriction_reasons.is_empty();
        let has_forward_count = self.forward_count > 0;
        let has_reply_info = !self.reply_info.is_empty();
        let has_sender_dialog_id = self.sender_dialog_id.is_valid();
        let has_top_thread_message_id = self.top_thread_message_id.is_valid();
        let has_thread_draft_message = self.thread_draft_message.is_some();
        let has_local_thread_message_ids = !self.local_thread_message_ids.is_empty();
        let has_linked_top_thread_message_id = self.linked_top_thread_message_id.is_valid();
        let has_interaction_info_update_date = self.interaction_info_update_date != 0;
        let has_send_emoji = !self.send_emoji.is_empty();
        let has_ttl_period = self.ttl_period != 0;
        let has_max_reply_media_timestamp = self.max_reply_media_timestamp >= 0;
        let are_message_media_timestamp_entities_found = true;
        let has_flags3 = true;
        let has_reactions = self.reactions.is_some();
        let has_available_reactions_generation = self.available_reactions_generation != 0;
        let has_history_generation = self.history_generation != 0;
        let is_reply_to_story = self.reply_to_story_full_id != StoryFullId::default();
        let has_input_reply_to = !self.message_id.is_any_server() && self.input_reply_to.is_valid();
        let has_replied_message_info = !self.replied_message_info.is_empty();
        let has_forward_info = self.forward_info.is_some();
        let has_saved_messages_topic_id = self.saved_messages_topic_id.is_valid();
        let has_initial_top_thread_message_id =
            !self.message_id.is_any_server() && self.initial_top_thread_message_id.is_valid();
        let has_sender_boost_count = self.sender_boost_count != 0;
        let has_via_business_bot_user_id = self.via_business_bot_user_id.is_valid();
        let has_effect_id = self.effect_id.is_valid();
        let has_fact_check = self.fact_check.is_some();
        let has_initial_sender_dialog_id =
            !self.message_id.is_any_server() && self.initial_sender_dialog_id.is_valid();
        let has_reactions_are_possible = true;
        let has_new_video_start_timestamp = self.new_video_start_timestamp >= 0;
        let has_paid_message_star_count = self.paid_message_star_count > 0;
        let has_suggested_post = self.suggested_post.is_some();
        let has_flags4 = true;
        let has_schedule_repeat_period = self.schedule_repeat_period != 0;
        let has_summary_from_language = !self.summary_from_language.is_empty();
        let has_sender_rank = !self.sender_rank.is_empty();
        let has_guest_bot_via_dialog_id = self.guest_bot_via_dialog_id.is_valid();
        let has_receiver_user_id = self.receiver_user_id.is_valid();
        let has_ephemeral_message_id = self.ephemeral_message_id.is_valid();
        let has_send_callback_query_id = self.send_callback_query_id != 0;
        let has_chat_instance = self.chat_instance != 0;
        let has_ephemeral_message = self.ephemeral_message.is_some();
        store_flags(
            out,
            [
                self.is_channel_post,
                self.is_outgoing,
                self.is_failed_to_send,
                self.disable_notification,
                self.contains_mention,
                self.from_background,
                self.disable_web_page_preview,
                self.clear_draft,
                false,
                false,
                has_sender_user_id,
                has_edit_date,
                has_random_id,
                false,
                false,
                is_reply_to_random_id,
                is_via_bot,
                has_view_count,
                has_reply_markup,
                has_ttl,
                has_author_signature,
                false,
                self.had_reply_markup,
                self.contains_unread_mention,
                has_media_album_id,
                false,
                self.in_game_share,
                self.is_content_secret,
                has_send_date,
                has_flags2,
            ],
        );
        if has_flags2 {
            store_flags(
                out,
                [
                    has_notification_id,
                    self.is_mention_notification_disabled,
                    self.had_forward_info,
                    false,
                    has_send_error_code,
                    self.hide_via_bot,
                    self.is_bot_start_message,
                    has_real_forward_from,
                    has_legacy_layer,
                    self.hide_edit_date,
                    has_restriction_reasons,
                    self.is_from_scheduled,
                    self.is_copy,
                    false,
                    has_forward_count,
                    has_reply_info,
                    has_sender_dialog_id,
                    false,
                    has_top_thread_message_id,
                    has_thread_draft_message,
                    has_local_thread_message_ids,
                    has_linked_top_thread_message_id,
                    self.is_pinned,
                    has_interaction_info_update_date,
                    has_send_emoji,
                    false,
                    has_ttl_period,
                    has_max_reply_media_timestamp,
                    are_message_media_timestamp_entities_found,
                    has_flags3,
                ],
            );
        }
        if has_flags3 {
            store_flags(
                out,
                [
                    self.noforwards,
                    self.has_explicit_sender,
                    has_reactions,
                    has_available_reactions_generation,
                    self.update_stickersets_order,
                    self.is_topic_message, // 5
                    has_history_generation,
                    is_reply_to_story,
                    false,
                    self.invert_media,
                    has_input_reply_to, // 10
                    has_replied_message_info,
                    has_forward_info,
                    has_saved_messages_topic_id,
                    has_initial_top_thread_message_id,
                    has_sender_boost_count, // 15
                    has_via_business_bot_user_id,
                    self.is_from_offline,
                    has_effect_id,
                    has_fact_check,
                    has_initial_sender_dialog_id, // 20
                    self.allow_paid,
                    self.video_processing_pending,
                    has_reactions_are_possible,
                    self.reactions_are_possible,
                    has_new_video_start_timestamp, // 25
                    has_paid_message_star_count,
                    has_suggested_post,
                    self.is_paid_suggested_post_stars,
                    has_flags4,
                ],
            );
        }
        if has_flags4 {
            store_flags(
                out,
                [
                    self.is_paid_suggested_post_ton,
                    self.initial_is_topic_message,
                    has_schedule_repeat_period,
                    has_summary_from_language,
                    self.is_quick_reply_message,
                    has_sender_rank,
                    has_guest_bot_via_dialog_id,
                    has_receiver_user_id,
                    has_ephemeral_message_id,
                    has_send_callback_query_id,
                    has_chat_instance,
                    self.send_anchor,
                    has_ephemeral_message,
                ],
            );
        }
        // update MessageDb::get_message_info when flags5 is added

        self.message_id.store(out);
        if has_sender_user_id {
            self.sender_user_id.store(out);
        }
        store_i32(out, self.date);
        if has_edit_date {
            store_i32(out, self.edit_date);
        }
        if has_send_date {
            store_i32(out, self.send_date);
        }
        if has_random_id {
            store_i64(out, self.random_id);
        }
        if has_forward_info {
            self.forward_info.as_ref().unwrap().store(out);
        }
        if has_real_forward_from {
            self.real_forward_from_dialog_id.store(out);
            self.real_forward_from_message_id.store(out);
        }
        if is_reply_to_random_id {
            store_i64(out, self.reply_to_random_id);
        }
        if is_via_bot {
            self.via_bot_user_id.store(out);
        }
        if has_view_count {
            store_i32(out, self.view_count);
        }
        if has_forward_count {
            store_i32(out, self.forward_count);
        }
        if has_reply_info {
            self.reply_info.store(out);
        }
        if has_ttl {
            self.ttl.store(out);
            crate::user_manager::TimeCtx::store_time(out, self.ttl_expires_at, time);
        }
        if has_send_error_code {
            store_i32(out, self.send_error_code);
            store_string(out, &self.send_error_message);
            if self.send_error_code == 429 {
                crate::user_manager::TimeCtx::store_time(out, self.try_resend_at, time);
            }
        }
        if has_author_signature {
            store_string(out, &self.author_signature);
        }
        if has_media_album_id {
            store_i64(out, self.media_album_id);
        }
        if has_notification_id {
            self.notification_id.store(out);
        }
        if has_legacy_layer {
            store_i32(out, self.legacy_layer);
        }
        if has_restriction_reasons {
            crate::restriction_reason::store_restriction_reasons(out, &self.restriction_reasons);
        }
        if has_sender_dialog_id {
            self.sender_dialog_id.store(out);
        }
        if has_top_thread_message_id {
            self.top_thread_message_id.store(out);
        }
        if has_thread_draft_message {
            self.thread_draft_message
                .as_ref()
                .unwrap()
                .store(storage, out);
        }
        if has_local_thread_message_ids {
            store_vec_header(out, self.local_thread_message_ids.len());
            for message_id in &self.local_thread_message_ids {
                message_id.store(out);
            }
        }
        if has_linked_top_thread_message_id {
            self.linked_top_thread_message_id.store(out);
        }
        if has_interaction_info_update_date {
            store_i32(out, self.interaction_info_update_date);
        }
        if has_send_emoji {
            store_string(out, &self.send_emoji);
        }
        crate::message_content::store_message_content(
            self.content.as_ref().expect("message content must be set"),
            storage,
            out,
        );
        if has_reply_markup {
            self.reply_markup.as_ref().unwrap().store(out);
        }
        if has_ttl_period {
            store_i32(out, self.ttl_period);
        }
        if has_max_reply_media_timestamp {
            store_i32(out, self.max_reply_media_timestamp);
        }
        if has_reactions {
            self.reactions.as_ref().unwrap().store(out);
        }
        if has_available_reactions_generation {
            store_i32(out, self.available_reactions_generation as i32);
        }
        if has_history_generation {
            store_i32(out, self.history_generation as i32);
        }
        if is_reply_to_story {
            self.reply_to_story_full_id.store(out);
        }
        if has_input_reply_to {
            self.input_reply_to.store(out);
        }
        if has_replied_message_info {
            self.replied_message_info.store(storage, out);
        }
        if has_saved_messages_topic_id {
            self.saved_messages_topic_id.store(out);
        }
        if has_initial_top_thread_message_id {
            self.initial_top_thread_message_id.store(out);
        }
        if has_sender_boost_count {
            store_i32(out, self.sender_boost_count);
        }
        if has_via_business_bot_user_id {
            self.via_business_bot_user_id.store(out);
        }
        if has_effect_id {
            self.effect_id.store(out);
        }
        if has_fact_check {
            self.fact_check.as_ref().unwrap().store(out);
        }
        if has_initial_sender_dialog_id {
            self.initial_sender_dialog_id.store(out);
        }
        if has_new_video_start_timestamp {
            store_i32(out, self.new_video_start_timestamp);
        }
        if has_paid_message_star_count {
            store_i64(out, self.paid_message_star_count);
        }
        if has_suggested_post {
            self.suggested_post.as_ref().unwrap().store(out);
        }
        if has_schedule_repeat_period {
            store_i32(out, self.schedule_repeat_period);
        }
        if has_summary_from_language {
            store_string(out, &self.summary_from_language);
        }
        if has_sender_rank {
            store_string(out, &self.sender_rank);
        }
        if has_guest_bot_via_dialog_id {
            self.guest_bot_via_dialog_id.store(out);
        }
        if has_receiver_user_id {
            self.receiver_user_id.store(out);
        }
        if has_ephemeral_message_id {
            self.ephemeral_message_id.store(out);
        }
        if has_send_callback_query_id {
            store_i64(out, self.send_callback_query_id);
        }
        if has_chat_instance {
            store_i64(out, self.chat_instance);
        }
        if has_ephemeral_message {
            self.ephemeral_message
                .as_ref()
                .unwrap()
                .store(storage, time, out);
        }
    }

    /// `MessagesManager::Message::parse` (MessagesManager.cpp:2658-3096)
    /// — the whole legacy ladder (forwarded/reply flags of every era) plus
    /// the closing repairs. "do not forget to resolve message dependencies".
    pub fn parse_from(
        parser: &mut Parser,
        storage: &mut dyn crate::message_content::MessageContentStorage,
        time: &crate::user_manager::TimeCtx,
    ) -> Result<Message, &'static str> {
        let mut m = Message::default();
        let mut has_notification_id = false;
        let [
            is_channel_post,
            is_outgoing,
            is_failed_to_send,
            disable_notification,
            contains_mention,
            from_background,
            disable_web_page_preview,
            clear_draft,
            _legacy_have_previous,
            _legacy_have_next,
            has_sender_user_id,
            has_edit_date,
            has_random_id,
            legacy_is_forwarded,
            legacy_is_reply,
            is_reply_to_random_id,
            is_via_bot,
            has_view_count,
            has_reply_markup,
            has_ttl,
            has_author_signature,
            legacy_has_forward_author_signature,
            had_reply_markup,
            contains_unread_mention,
            has_media_album_id,
            legacy_has_forward_from,
            in_game_share,
            is_content_secret,
            has_send_date,
            has_flags2,
        ] = parser.parse_flags::<30>()?;
        let mut legacy_has_forward_sender_name = false;
        let mut has_send_error_code = false;
        let mut has_real_forward_from = false;
        let mut has_legacy_layer = false;
        let mut has_restriction_reasons = false;
        let mut legacy_has_forward_psa_type = false;
        let mut has_forward_count = false;
        let mut has_reply_info = false;
        let mut has_sender_dialog_id = false;
        let mut legacy_has_reply_in_dialog_id = false;
        let mut has_top_thread_message_id = false;
        let mut has_thread_draft_message = false;
        let mut has_local_thread_message_ids = false;
        let mut has_linked_top_thread_message_id = false;
        let mut has_interaction_info_update_date = false;
        let mut has_send_emoji = false;
        let mut legacy_is_imported = false;
        let mut has_ttl_period = false;
        let mut has_max_reply_media_timestamp = false;
        let mut has_flags3 = false;
        if has_flags2 {
            let [
                has_notification_id_,
                is_mention_notification_disabled,
                had_forward_info,
                legacy_has_forward_sender_name_,
                has_send_error_code_,
                hide_via_bot,
                is_bot_start_message,
                has_real_forward_from_,
                has_legacy_layer_,
                hide_edit_date,
                has_restriction_reasons_,
                is_from_scheduled,
                is_copy,
                legacy_has_forward_psa_type_,
                has_forward_count_,
                has_reply_info_,
                has_sender_dialog_id_,
                legacy_has_reply_in_dialog_id_,
                has_top_thread_message_id_,
                has_thread_draft_message_,
                has_local_thread_message_ids_,
                has_linked_top_thread_message_id_,
                is_pinned,
                has_interaction_info_update_date_,
                has_send_emoji_,
                legacy_is_imported_,
                has_ttl_period_,
                has_max_reply_media_timestamp_,
                are_media_timestamp_entities_found,
                has_flags3_,
            ] = parser.parse_flags::<30>()?;
            has_notification_id = has_notification_id_;
            m.is_mention_notification_disabled = is_mention_notification_disabled;
            m.had_forward_info = had_forward_info;
            legacy_has_forward_sender_name = legacy_has_forward_sender_name_;
            has_send_error_code = has_send_error_code_;
            m.hide_via_bot = hide_via_bot;
            m.is_bot_start_message = is_bot_start_message;
            has_real_forward_from = has_real_forward_from_;
            has_legacy_layer = has_legacy_layer_;
            m.hide_edit_date = hide_edit_date;
            has_restriction_reasons = has_restriction_reasons_;
            m.is_from_scheduled = is_from_scheduled;
            m.is_copy = is_copy;
            legacy_has_forward_psa_type = legacy_has_forward_psa_type_;
            has_forward_count = has_forward_count_;
            has_reply_info = has_reply_info_;
            has_sender_dialog_id = has_sender_dialog_id_;
            legacy_has_reply_in_dialog_id = legacy_has_reply_in_dialog_id_;
            has_top_thread_message_id = has_top_thread_message_id_;
            has_thread_draft_message = has_thread_draft_message_;
            has_local_thread_message_ids = has_local_thread_message_ids_;
            has_linked_top_thread_message_id = has_linked_top_thread_message_id_;
            m.is_pinned = is_pinned;
            has_interaction_info_update_date = has_interaction_info_update_date_;
            has_send_emoji = has_send_emoji_;
            legacy_is_imported = legacy_is_imported_;
            has_ttl_period = has_ttl_period_;
            has_max_reply_media_timestamp = has_max_reply_media_timestamp_;
            m.are_media_timestamp_entities_found = are_media_timestamp_entities_found;
            has_flags3 = has_flags3_;
        }
        let mut has_reactions = false;
        let mut has_available_reactions_generation = false;
        let mut has_history_generation = false;
        let mut is_reply_to_story = false;
        let mut legacy_has_forward_origin = false;
        let mut has_input_reply_to = false;
        let mut has_replied_message_info = false;
        let mut has_forward_info = false;
        let mut has_saved_messages_topic_id = false;
        let mut has_initial_top_thread_message_id = false;
        let mut has_sender_boost_count = false;
        let mut has_via_business_bot_user_id = false;
        let mut has_effect_id = false;
        let mut has_fact_check = false;
        let mut has_initial_sender_dialog_id = false;
        let mut has_reactions_are_possible = false;
        let mut has_new_video_start_timestamp = false;
        let mut has_paid_message_star_count = false;
        let mut has_suggested_post = false;
        let mut has_flags4 = false;
        let mut has_schedule_repeat_period = false;
        let mut has_summary_from_language = false;
        let mut has_sender_rank = false;
        let mut has_guest_bot_via_dialog_id = false;
        let mut has_receiver_user_id = false;
        let mut has_ephemeral_message_id = false;
        let mut has_send_callback_query_id = false;
        let mut has_chat_instance = false;
        let mut has_ephemeral_message = false;
        if has_flags3 {
            let [
                noforwards,
                has_explicit_sender,
                has_reactions_,
                has_available_reactions_generation_,
                update_stickersets_order,
                is_topic_message,
                has_history_generation_,
                is_reply_to_story_,
                legacy_has_forward_origin_,
                invert_media,
                has_input_reply_to_,
                has_replied_message_info_,
                has_forward_info_,
                has_saved_messages_topic_id_,
                has_initial_top_thread_message_id_,
                has_sender_boost_count_,
                has_via_business_bot_user_id_,
                is_from_offline,
                has_effect_id_,
                has_fact_check_,
                has_initial_sender_dialog_id_,
                allow_paid,
                video_processing_pending,
                has_reactions_are_possible_,
                reactions_are_possible,
                has_new_video_start_timestamp_,
                has_paid_message_star_count_,
                has_suggested_post_,
                is_paid_suggested_post_stars,
                has_flags4_,
            ] = parser.parse_flags::<30>()?;
            m.noforwards = noforwards;
            m.has_explicit_sender = has_explicit_sender;
            has_reactions = has_reactions_;
            has_available_reactions_generation = has_available_reactions_generation_;
            m.update_stickersets_order = update_stickersets_order;
            m.is_topic_message = is_topic_message;
            has_history_generation = has_history_generation_;
            is_reply_to_story = is_reply_to_story_;
            legacy_has_forward_origin = legacy_has_forward_origin_;
            m.invert_media = invert_media;
            has_input_reply_to = has_input_reply_to_;
            has_replied_message_info = has_replied_message_info_;
            has_forward_info = has_forward_info_;
            has_saved_messages_topic_id = has_saved_messages_topic_id_;
            has_initial_top_thread_message_id = has_initial_top_thread_message_id_;
            has_sender_boost_count = has_sender_boost_count_;
            has_via_business_bot_user_id = has_via_business_bot_user_id_;
            m.is_from_offline = is_from_offline;
            has_effect_id = has_effect_id_;
            has_fact_check = has_fact_check_;
            has_initial_sender_dialog_id = has_initial_sender_dialog_id_;
            m.allow_paid = allow_paid;
            m.video_processing_pending = video_processing_pending;
            has_reactions_are_possible = has_reactions_are_possible_;
            m.reactions_are_possible = reactions_are_possible;
            has_new_video_start_timestamp = has_new_video_start_timestamp_;
            has_paid_message_star_count = has_paid_message_star_count_;
            has_suggested_post = has_suggested_post_;
            m.is_paid_suggested_post_stars = is_paid_suggested_post_stars;
            has_flags4 = has_flags4_;
        }
        if has_flags4 {
            let [
                is_paid_suggested_post_ton,
                initial_is_topic_message,
                has_schedule_repeat_period_,
                has_summary_from_language_,
                is_quick_reply_message,
                has_sender_rank_,
                has_guest_bot_via_dialog_id_,
                has_receiver_user_id_,
                has_ephemeral_message_id_,
                has_send_callback_query_id_,
                has_chat_instance_,
                send_anchor,
                has_ephemeral_message_,
            ] = parser.parse_flags::<13>()?;
            has_schedule_repeat_period = has_schedule_repeat_period_;
            has_summary_from_language = has_summary_from_language_;
            has_sender_rank = has_sender_rank_;
            has_guest_bot_via_dialog_id = has_guest_bot_via_dialog_id_;
            has_receiver_user_id = has_receiver_user_id_;
            has_ephemeral_message_id = has_ephemeral_message_id_;
            has_send_callback_query_id = has_send_callback_query_id_;
            has_chat_instance = has_chat_instance_;
            has_ephemeral_message = has_ephemeral_message_;
            m.is_paid_suggested_post_ton = is_paid_suggested_post_ton;
            m.initial_is_topic_message = initial_is_topic_message;
            m.is_quick_reply_message = is_quick_reply_message;
            m.send_anchor = send_anchor;
        }

        m.is_channel_post = is_channel_post;
        m.is_outgoing = is_outgoing;
        m.is_failed_to_send = is_failed_to_send;
        m.disable_notification = disable_notification;
        m.contains_mention = contains_mention;
        m.from_background = from_background;
        m.disable_web_page_preview = disable_web_page_preview;
        m.clear_draft = clear_draft;
        m.had_reply_markup = had_reply_markup;
        m.contains_unread_mention = contains_unread_mention;
        m.in_game_share = in_game_share;
        m.is_content_secret = is_content_secret;

        m.message_id = MessageId::parse_from(parser)?;
        if !m.message_id.is_valid() && !m.message_id.is_valid_scheduled() {
            return Err("Invalid message identifier");
        }
        if has_sender_user_id {
            m.sender_user_id = UserId::parse_from(parser)?;
        }
        m.date = parser.i32_()?;
        if has_edit_date {
            m.edit_date = parser.i32_()?;
        }
        if has_send_date {
            if !m.message_id.is_yet_unsent() {
                return Err("Unexpected send date");
            }
            m.send_date = parser.i32_()?;
        } else if m.message_id.is_valid() && m.message_id.is_yet_unsent() {
            m.send_date = m.date; // for backward compatibility
        }
        if has_random_id {
            m.random_id = parser.i64_()?;
        }
        if has_forward_info {
            m.forward_info = Some(MessageForwardInfo::parse_from(parser)?);
        } else if legacy_is_forwarded {
            let (forward_origin, forward_date) = if legacy_has_forward_origin {
                (MessageOrigin::parse_from(parser)?, parser.i32_()?)
            } else {
                let forward_sender_user_id = UserId::parse_from(parser)?;
                let forward_date = parser.i32_()?;
                let forward_sender_dialog_id = DialogId::parse_from(parser)?;
                let forward_message_id = MessageId::parse_from(parser)?;
                let forward_author_signature = if legacy_has_forward_author_signature {
                    parser.string()?
                } else {
                    String::new()
                };
                let forward_sender_name = if legacy_has_forward_sender_name {
                    parser.string()?
                } else {
                    String::new()
                };
                (
                    MessageOrigin::new(
                        forward_sender_user_id,
                        forward_sender_dialog_id,
                        forward_message_id,
                        forward_author_signature,
                        forward_sender_name,
                    ),
                    forward_date,
                )
            };
            let mut last_message_info = LastForwardedMessageInfo::default();
            if legacy_has_forward_from {
                let forward_from_dialog_id = DialogId::parse_from(parser)?;
                let forward_from_message_id = MessageId::parse_from(parser)?;
                last_message_info = LastForwardedMessageInfo {
                    dialog_id: forward_from_dialog_id,
                    message_id: forward_from_message_id,
                    sender_dialog_id: DialogId::default(),
                    sender_name: String::new(),
                    date: 0,
                    is_outgoing: false,
                };
            }
            let psa_type = if legacy_has_forward_psa_type {
                parser.string()?
            } else {
                String::new()
            };
            m.forward_info = Some(MessageForwardInfo {
                origin: forward_origin,
                date: forward_date,
                last_message_info,
                psa_type,
                is_imported: legacy_is_imported,
            });
        }
        if has_real_forward_from {
            m.real_forward_from_dialog_id = DialogId::parse_from(parser)?;
            m.real_forward_from_message_id = MessageId::parse_from(parser)?;
        }
        let mut legacy_reply_to_message_id = MessageId::default();
        if legacy_is_reply {
            legacy_reply_to_message_id = MessageId::parse_from(parser)?;
        }
        if is_reply_to_random_id {
            m.reply_to_random_id = parser.i64_()?;
        }
        if is_via_bot {
            m.via_bot_user_id = UserId::parse_from(parser)?;
        }
        if has_view_count {
            m.view_count = parser.i32_()?;
        }
        if has_forward_count {
            m.forward_count = parser.i32_()?;
        }
        if has_reply_info {
            m.reply_info = MessageReplyInfo::parse_from(parser)?;
        }
        if has_ttl {
            m.ttl = MessageSelfDestructType::parse_from(parser)?;
            m.ttl_expires_at = crate::user_manager::TimeCtx::parse_time(parser, time)?;
        }
        if has_send_error_code {
            m.send_error_code = parser.i32_()?;
            m.send_error_message = parser.string()?;
            if m.send_error_code == 429 {
                m.try_resend_at = crate::user_manager::TimeCtx::parse_time(parser, time)?;
            }
        }
        if has_author_signature {
            m.author_signature = parser.string()?;
        }
        if has_media_album_id {
            m.media_album_id = parser.i64_()?;
        }
        if has_notification_id {
            m.notification_id = NotificationId::parse_from(parser)?;
        }
        if has_legacy_layer {
            m.legacy_layer = parser.i32_()?;
        }
        if has_restriction_reasons {
            m.restriction_reasons = crate::restriction_reason::parse_restriction_reasons(parser)?;
        }
        if has_sender_dialog_id {
            m.sender_dialog_id = DialogId::parse_from(parser)?;
        }
        let mut legacy_reply_in_dialog_id = DialogId::default();
        if legacy_has_reply_in_dialog_id {
            legacy_reply_in_dialog_id = DialogId::parse_from(parser)?;
        }
        if has_top_thread_message_id {
            m.top_thread_message_id = MessageId::parse_from(parser)?;
        }
        if has_thread_draft_message {
            m.thread_draft_message = Some(DraftMessage::parse_from(parser, storage)?);
        }
        if has_local_thread_message_ids {
            let size = parser.i32_()?;
            if size < 0 || size as usize > parser.left_len() / 4 {
                return Err("Wrong vector length");
            }
            for _ in 0..size {
                m.local_thread_message_ids
                    .push(MessageId::parse_from(parser)?);
            }
        }
        if has_linked_top_thread_message_id {
            m.linked_top_thread_message_id = MessageId::parse_from(parser)?;
        }
        if has_interaction_info_update_date {
            m.interaction_info_update_date = parser.i32_()?;
        }
        if has_send_emoji {
            m.send_emoji = parser.string()?;
        }
        m.content = Some(crate::message_content::parse_message_content(
            parser, storage,
        )?);
        if has_reply_markup {
            m.reply_markup = Some(Box::new(ReplyMarkup::parse_from(parser)?));
        }
        if has_ttl_period {
            m.ttl_period = parser.i32_()?;
        }
        if has_max_reply_media_timestamp {
            m.max_reply_media_timestamp = parser.i32_()?;
        }
        if has_reactions {
            m.reactions = Some(MessageReactions::parse_from(parser)?);
        }
        if has_available_reactions_generation {
            m.available_reactions_generation = parser.i32_()? as u32;
        }
        if has_history_generation {
            m.history_generation = parser.i32_()? as u32;
        }
        if is_reply_to_story {
            m.reply_to_story_full_id = StoryFullId::parse_from(parser)?;
        }
        if has_input_reply_to {
            m.input_reply_to = MessageInputReplyTo::parse_from(parser)?;
        } else if !m.message_id.is_any_server() {
            if m.reply_to_story_full_id.is_valid() {
                m.input_reply_to = MessageInputReplyTo {
                    story_full_id: m.reply_to_story_full_id,
                    ..MessageInputReplyTo::default()
                };
            } else if legacy_reply_to_message_id.is_valid() {
                m.input_reply_to = MessageInputReplyTo::regular(legacy_reply_to_message_id);
            }
        }
        if has_replied_message_info {
            m.replied_message_info = RepliedMessageInfo::parse_from(parser, storage)?;
        } else {
            m.replied_message_info =
                RepliedMessageInfo::legacy(legacy_reply_to_message_id, legacy_reply_in_dialog_id);
        }
        if has_saved_messages_topic_id {
            m.saved_messages_topic_id = SavedMessagesTopicId::parse_from(parser)?;
        }
        if has_initial_top_thread_message_id {
            m.initial_top_thread_message_id = MessageId::parse_from(parser)?;
        }
        if has_sender_boost_count {
            m.sender_boost_count = parser.i32_()?;
        }
        if has_via_business_bot_user_id {
            m.via_business_bot_user_id = UserId::parse_from(parser)?;
        }
        if has_effect_id {
            m.effect_id = MessageEffectId::parse_from(parser)?;
        }
        if has_fact_check {
            m.fact_check = Some(FactCheck::parse_from(parser)?);
        }
        if has_initial_sender_dialog_id {
            m.initial_sender_dialog_id = DialogId::parse_from(parser)?;
        }
        if has_new_video_start_timestamp {
            m.new_video_start_timestamp = parser.i32_()?;
        } else {
            m.new_video_start_timestamp = -1;
        }
        if has_paid_message_star_count {
            m.paid_message_star_count = parser.i64_()?;
        }
        if has_suggested_post {
            m.suggested_post = Some(SuggestedPost::parse_from(parser)?);
        }
        if has_schedule_repeat_period {
            m.schedule_repeat_period = parser.i32_()?;
        }
        if has_summary_from_language {
            m.summary_from_language = parser.string()?;
        }
        if has_sender_rank {
            m.sender_rank = parser.string()?;
        }
        if has_guest_bot_via_dialog_id {
            m.guest_bot_via_dialog_id = DialogId::parse_from(parser)?;
        }
        if has_receiver_user_id {
            m.receiver_user_id = UserId::parse_from(parser)?;
            if !m.receiver_user_id.is_valid() {
                return Err("Invalid ephemeral message receiver identifier");
            }
        }
        if has_ephemeral_message_id {
            m.ephemeral_message_id = EphemeralMessageId::parse_from(parser)?;
            if !m.ephemeral_message_id.is_valid() {
                return Err("Invalid ephemeral message identifier");
            }
        }
        if has_send_callback_query_id {
            m.send_callback_query_id = parser.i64_()?;
        }
        if has_chat_instance {
            m.chat_instance = parser.i64_()?;
        }
        if has_ephemeral_message {
            m.ephemeral_message = Some(Box::new(Message::parse_from(parser, storage, time)?));
        }

        let content = m.content.as_ref().unwrap();
        m.is_content_secret |= self_destruct_is_secret(m.ttl, content.get_type()); // repair is_content_secret for old messages
        if m.hide_edit_date && content.get_type() == MessageContentType::LiveLocation {
            m.hide_edit_date = false;
        }
        if !has_reactions_are_possible {
            m.reactions_are_possible =
                get_default_service_message_content_reactions_are_possible(content.get_type());
        }
        Ok(m)
    }
}

/// `log_event_store(Message)` — the version-prefixed database value
/// (MessagesManager.cpp:31879/31924 `log_event_store(*m)`).
pub fn store_message_log_event(
    message: &Message,
    storage: &dyn crate::message_content::MessageContentStorage,
    time: &crate::user_manager::TimeCtx,
    out: &mut Vec<u8>,
) {
    crate::store_parse::store_log_event_version(out);
    message.store(storage, time, out);
}

/// `log_event_parse(Message, value)` — the load side of the message
/// database row / binlog event.
pub fn parse_message_log_event(
    input: &[u8],
    storage: &mut dyn crate::message_content::MessageContentStorage,
    time: &crate::user_manager::TimeCtx,
) -> Result<Message, &'static str> {
    let mut parser = Parser::new_log_event(input)?;
    let message = Message::parse_from(&mut parser, storage, time)?;
    parser.fetch_end()?;
    Ok(message)
}

/// The `Message::parse` closing-repair helper: the C++ inline
/// `ttl.is_secret_message_content(content->get_type())` call
/// (MessagesManager.cpp:3089).
fn self_destruct_is_secret(ttl: MessageSelfDestructType, content_type: MessageContentType) -> bool {
    ttl.is_secret_message_content(content_type)
}

// ---------------------------------------------------------------------------
// M.10.2c — the manager skeleton + the database family
// (MessagesManager.cpp:31866-32152, 4161-4197, 5005-5023, 8138-8144,
// 19525-19620, 3749-3808) over the M.6 MessageDb engines. The manager struct
// carries the core container set; per-slice state grows with its tasks (the
// plan convention). Seams:
// - `DialogReactionsContext::is_chat_banned` — the Chat/Channel banned check
//   needs the chat-status plane; the Td impl lands with its slice.
// - `get_active_reactions` filters through the manager's
//   `active_reaction_pos` cache (the Wave-2 reactions state) — empty for
//   now, recorded.
// - `get_message_search_text` rides `get_message_content_search_text`
//   (MessageContent.cpp), which lands with the M.9.17 lifecycle tail on the
//   sibling line — empty string until that merge.
// ---------------------------------------------------------------------------

/// The context the reactions-visibility predicates need beyond the pure
/// values (MessagesManager.cpp:27558-27571 `need_hide_dialog_reactions`).
pub trait DialogReactionsContext {
    /// `ChatManager::get_chat_status(chat_id).is_banned()` /
    /// `get_channel_status(channel_id).is_banned()` — false for the other
    /// dialog types.
    fn is_chat_banned(&self, dialog_id: DialogId) -> bool;

    /// `AuthManager::is_bot()`.
    fn is_bot(&self) -> bool;
}

/// The no-context stand-in: nothing banned, not a bot (tests).
pub struct NoDialogReactionsContext;

impl DialogReactionsContext for NoDialogReactionsContext {
    fn is_chat_banned(&self, _dialog_id: DialogId) -> bool {
        false
    }

    fn is_bot(&self) -> bool {
        false
    }
}

/// `MessagesManager::CallsDbState` (MessagesManager.h:1604-1610).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CallsDbState {
    pub first_calls_database_message_id_by_index: [MessageId; 2],
    pub message_count_by_index: [i32; 2],
}

impl CallsDbState {
    /// `CallsDbState::store` (MessagesManager.cpp:3749-3760).
    pub fn store(&self, out: &mut Vec<u8>) {
        store_i32(
            out,
            self.first_calls_database_message_id_by_index.len() as i32,
        );
        for first_message_id in &self.first_calls_database_message_id_by_index {
            first_message_id.store(out);
        }
        store_i32(out, self.message_count_by_index.len() as i32);
        for message_count in &self.message_count_by_index {
            store_i32(out, *message_count);
        }
    }

    /// `CallsDbState::parse` (MessagesManager.cpp:3762-3780).
    pub fn parse_from(parser: &mut Parser) -> Result<CallsDbState, &'static str> {
        let size = parser.i32_()?;
        if size < 0 || size as usize > 2 {
            return Err("Wrong first_calls_database_message_id_by_index table size");
        }
        let mut state = CallsDbState::default();
        for entry in state
            .first_calls_database_message_id_by_index
            .iter_mut()
            .take(size as usize)
        {
            *entry = MessageId::parse_from(parser)?;
        }
        let size = parser.i32_()?;
        if size < 0 || size as usize > 2 {
            return Err("Wrong message_count_by_index table size");
        }
        for entry in state.message_count_by_index.iter_mut().take(size as usize) {
            *entry = parser.i32_()?;
        }
        Ok(state)
    }
}

/// The `td::MessagesManager` struct skeleton (MessagesManager.h:3300+): the
/// per-dialog container set the database family and the receive path walk.
/// The C++ class carries ~200 private fields; the ones the ported slices
/// touch land here per slice, the rest grow with their owning tasks.
#[derive(Debug, Default)]
pub struct MessagesManager {
    /// `dialogs_` (MessagesManager.h:2948).
    pub(crate) dialogs: HashMap<DialogId, Box<Dialog>>,
    /// `update_message_ids_` — new_message_id -> temporary_id.
    pub(crate) update_message_ids: HashMap<(DialogId, MessageId), MessageId>,
    // `being_sent_messages_` — message_random_id -> message; the send path
    // (M.10.8+) reads it
    #[allow(dead_code)]
    pub(crate) being_sent_messages: HashMap<i64, MessageFullId>,
    // `being_readded_message_full_id_` — the delete/files path reads it
    #[allow(dead_code)]
    pub(crate) being_readded_message_full_id: MessageFullId,
    /// `calls_db_state_` (MessagesManager.h:1600).
    pub(crate) calls_db_state: CallsDbState,
    /// `active_reaction_pos_` — the Wave-2 reactions-manager cache; empty
    /// until that slice ([blocked: Wave-2]).
    pub(crate) active_reaction_pos: HashMap<ReactionType, usize>,
}

impl MessagesManager {
    pub fn new() -> MessagesManager {
        MessagesManager::default()
    }

    /// `MessagesManager::get_dialog` (the map lookup form).
    pub fn get_dialog(&self, dialog_id: DialogId) -> Option<&Dialog> {
        self.dialogs.get(&dialog_id).map(|d| d.as_ref())
    }

    pub fn get_dialog_mut(&mut self, dialog_id: DialogId) -> Option<&mut Dialog> {
        self.dialogs.get_mut(&dialog_id).map(|d| d.as_mut())
    }

    /// `MessagesManager::load_calls_db_state` (MessagesManager.cpp:3782-3796)
    /// — the `calls_db_state` pmc row, -1 counts until loaded.
    pub fn load_calls_db_state(&mut self, td_db_is_message_database: bool) {
        // the sqlite sync pmc read lands with the Td wiring slice; the
        // in-memory -1 seeding is the loadable part
        if !td_db_is_message_database {
            return;
        }
        self.calls_db_state.message_count_by_index = [-1, -1];
    }
}

/// `MessagesManager::get_message_sender` (MessagesManager.cpp:6200-6204).
pub fn get_message_sender(m: &Message) -> DialogId {
    if m.sender_dialog_id.is_valid() {
        m.sender_dialog_id
    } else {
        DialogId::from_user_id(m.sender_user_id)
    }
}

/// `MessagesManager::get_message_search_text` (MessagesManager.cpp:8138-8143)
/// — [blocked: M.9.17] `get_message_content_search_text` lands with the
/// lifecycle tail; the empty string is the recorded stand-in.
pub fn get_message_search_text(m: &Message) -> String {
    if m.is_content_secret {
        return String::new();
    }
    String::new()
}

/// `MessagesManager::is_discussion_message` (MessagesManager.cpp:23913-23934)
/// — the linked-dialog lookups ride the manager maps.
pub fn is_discussion_message(
    manager: &MessagesManager,
    ctx: &dyn DialogReactionsContext,
    dialog_id: DialogId,
    m: &Message,
) -> bool {
    let Some(forward_info) = &m.forward_info else {
        return false;
    };
    if m.sender_user_id.is_valid() {
        // only the service notifications bot posts discussion messages as a
        // user
        if !ctx.is_bot()
            || m.sender_user_id.get()
                != crate::user_manager::UserManager::get_service_notifications_user_id().get()
        {
            return false;
        }
    }
    let linked_dialog_id = forward_info.get_last_dialog_id();
    if !linked_dialog_id.is_valid() || linked_dialog_id == dialog_id {
        return false;
    }
    match dialog_id.get_type() {
        DialogType::User => {
            linked_dialog_id.get_type() == DialogType::Channel
                && manager
                    .get_dialog(linked_dialog_id)
                    .is_some_and(|d| d.is_forum)
        }
        DialogType::Channel => {
            // is_megagroup_channel of the dialog — the channel kind rides
            // the chat status; false until that slice ([blocked: M.5 seam])
            false
        }
        _ => false,
    }
}

/// `MessagesManager::get_active_reactions` (MessagesManager.cpp:6416-6421).
pub fn get_active_reactions(
    manager: &MessagesManager,
    ctx: &dyn DialogReactionsContext,
    available_reactions: &ChatReactions,
) -> ChatReactions {
    if ctx.is_bot() {
        return available_reactions.clone();
    }
    // `ChatReactions::get_active_reactions` (ChatReactions.cpp) — filters
    // the reaction set to the active ones; with the empty Wave-2 cache the
    // paid flag survives and the regular set empties
    let mut result = available_reactions.clone();
    result
        .reaction_types
        .retain(|reaction_type| manager.active_reaction_pos.contains_key(reaction_type));
    result.allow_all_regular = false;
    result.allow_all_custom = false;
    result
}

/// `MessagesManager::need_hide_dialog_reactions`
/// (MessagesManager.cpp:27558-27571).
pub fn need_hide_dialog_reactions(ctx: &dyn DialogReactionsContext, dialog_id: DialogId) -> bool {
    match dialog_id.get_type() {
        DialogType::User | DialogType::SecretChat => false,
        DialogType::Chat | DialogType::Channel => ctx.is_chat_banned(dialog_id),
        DialogType::None => false,
    }
}

/// `MessagesManager::get_dialog_active_reactions`
/// (MessagesManager.cpp:6423-6440).
pub fn get_dialog_active_reactions(
    manager: &MessagesManager,
    ctx: &dyn DialogReactionsContext,
    d: &Dialog,
) -> ChatReactions {
    if need_hide_dialog_reactions(ctx, d.dialog_id) {
        return ChatReactions::default();
    }
    match d.dialog_id.get_type() {
        DialogType::User => ChatReactions::from_all_flags(true, true),
        DialogType::Chat | DialogType::Channel => {
            get_active_reactions(manager, ctx, &d.available_reactions)
        }
        _ => ChatReactions::default(),
    }
}

/// `MessagesManager::get_message_active_reactions`
/// (MessagesManager.cpp:6391-6414).
pub fn get_message_active_reactions(
    manager: &MessagesManager,
    ctx: &dyn DialogReactionsContext,
    d: &Dialog,
    m: &Message,
) -> ChatReactions {
    if !m.message_id.is_server() {
        return ChatReactions::default();
    }
    if !m.reactions_are_possible
        && crate::message_content::is_service_message_content(
            m.content.as_ref().unwrap().get_type(),
        )
    {
        return ChatReactions::default();
    }
    if is_discussion_message(manager, ctx, d.dialog_id, m) {
        let linked_dialog_id = m.forward_info.as_ref().unwrap().last_message_info.dialog_id;
        return match manager.get_dialog(linked_dialog_id) {
            Some(linked_d) => get_dialog_active_reactions(manager, ctx, linked_d),
            None => ChatReactions::default(),
        };
    }
    get_dialog_active_reactions(manager, ctx, d)
}

/// `MessagesManager::has_unread_message_reactions`
/// (MessagesManager.cpp:5005-5012).
pub fn has_unread_message_reactions(
    manager: &MessagesManager,
    ctx: &dyn DialogReactionsContext,
    dialog_id: DialogId,
    m: &Message,
) -> bool {
    if ctx.is_bot() {
        return false;
    }
    let Some(reactions) = &m.reactions else {
        return false;
    };
    if reactions.unread_reactions.is_empty() {
        return false;
    }
    // `is_visible_message_reactions` (MessagesManager.cpp:6370-6390)
    let Some(d) = manager.get_dialog(dialog_id) else {
        return false;
    };
    if get_message_active_reactions(manager, ctx, d, m).empty() {
        return false;
    }
    m.available_reactions_generation == d.available_reactions_generation
}

/// `MessagesManager::has_unread_poll_votes` (MessagesManager.cpp:5014-5023).
pub fn has_unread_poll_votes(ctx: &dyn DialogReactionsContext, m: &Message) -> bool {
    if ctx.is_bot() {
        return false;
    }
    if m.forward_info.is_some()
        || m.content.as_ref().unwrap().get_type() != MessageContentType::Poll
    {
        return false;
    }
    false // [blocked: M.9.17] get_message_content_poll_has_unread_votes over the poll manager seam
}

/// `MessagesManager::get_message_index_mask` (MessagesManager.cpp:4161-4197).
pub fn get_message_index_mask(
    manager: &MessagesManager,
    ctx: &dyn DialogReactionsContext,
    content_ctx: &dyn crate::message_content::MessageContentContext,
    dialog_id: DialogId,
    m: &Message,
) -> i32 {
    if ctx.is_bot() || m.message_id.is_scheduled() || m.message_id.is_yet_unsent() {
        return 0;
    }
    if m.is_failed_to_send {
        return message_search_filter_index_mask(MessageSearchFilter::FailedToSend);
    }
    let is_secret = dialog_id.get_type() == DialogType::SecretChat;
    if !m.message_id.is_server() && !is_secret {
        return 0;
    }

    let mut index_mask = 0;
    if m.is_pinned {
        index_mask |= message_search_filter_index_mask(MessageSearchFilter::Pinned);
    }
    // retain second condition just in case
    if m.is_content_secret || (!m.ttl.is_empty() && !is_secret) {
        return index_mask;
    }
    index_mask |= crate::message_content::get_message_content_index_mask(
        content_ctx,
        m.content.as_ref().unwrap(),
        m.is_outgoing,
    );
    if m.contains_mention {
        index_mask |= message_search_filter_index_mask(MessageSearchFilter::Mention);
        if m.contains_unread_mention {
            index_mask |= message_search_filter_index_mask(MessageSearchFilter::UnreadMention);
        }
    }
    if has_unread_message_reactions(manager, ctx, dialog_id, m) {
        index_mask |= message_search_filter_index_mask(MessageSearchFilter::UnreadReaction);
    }
    if has_unread_poll_votes(ctx, m) {
        index_mask |= message_search_filter_index_mask(MessageSearchFilter::UnreadPollVote);
    }
    index_mask
}

/// `MessagesManager::add_message_to_database` (MessagesManager.cpp:31866-31929)
/// — over the M.6 async engine. The `time` bundle feeds the
/// `ttl_expires_at` relative conversion.
#[allow(clippy::too_many_arguments)]
pub fn add_message_to_database(
    global: &crate::global::Global,
    manager: &mut MessagesManager,
    reactions_ctx: &dyn DialogReactionsContext,
    content_ctx: &dyn crate::message_content::MessageContentContext,
    time: &crate::user_manager::TimeCtx,
    dialog_id: DialogId,
    m: &Message,
    source: &str,
) {
    if !global.use_message_database() {
        return;
    }

    let message_id = m.message_id;

    if message_id.is_scheduled() {
        tracing::info!(
            "Add message {}:{} to database from {}",
            dialog_id.get(),
            message_id.get(),
            source
        );
        // `set_dialog_has_scheduled_database_messages` rides the dialog
        // state slice (M.10.3); the engine write is the loadable half
        let mut data = Vec::new();
        store_message_log_event(m, &MessageDbStorageBridge, time, &mut data);
        global.message_db_async().add_scheduled_message(
            MessageFullId::new(dialog_id, message_id),
            data,
            tdutils::promise::PromiseCreator::lambda(|()| ()),
        );
        return;
    }
    debug_assert!(message_id.is_server() || message_id.is_local());

    tracing::info!(
        "Add message {}:{} to database from {}",
        dialog_id.get(),
        message_id.get(),
        source
    );

    let mut unique_message_id = ServerMessageId::new(0);
    let mut random_id = 0i64;
    let mut search_id = 0i64;
    let mut text = String::new();
    match dialog_id.get_type() {
        DialogType::User | DialogType::Chat => {
            if message_id.is_server() {
                unique_message_id = message_id.get_server_message_id();
            }
        }
        DialogType::Channel => {}
        DialogType::SecretChat => {
            random_id = m.random_id;
            text = get_message_search_text(m);
            if !text.is_empty() {
                search_id = ((m.date as i64) << 32) | (m.random_id as u32 as i64);
            }
        }
        DialogType::None => {
            unreachable!()
        }
    }

    let mut ttl_expires_at = 0i32;
    if m.ttl_expires_at != 0.0 {
        ttl_expires_at = (m.ttl_expires_at - time.now + time.server_time) as i32 + 1;
    }
    if m.ttl_period != 0 && (ttl_expires_at == 0 || m.date + m.ttl_period < ttl_expires_at) {
        ttl_expires_at = m.date + m.ttl_period;
    }
    let d = manager
        .dialogs
        .get(&dialog_id)
        .expect("dialog must exist when adding a message to the database");
    let index_mask = get_message_index_mask(manager, reactions_ctx, content_ctx, dialog_id, m);
    let _ = d;
    let mut data = Vec::new();
    store_message_log_event(m, &MessageDbStorageBridge, time, &mut data);
    global.message_db_async().add_message(
        MessageFullId::new(dialog_id, message_id),
        unique_message_id,
        get_message_sender(m),
        random_id,
        ttl_expires_at,
        index_mask,
        search_id,
        text,
        m.notification_id,
        m.top_thread_message_id,
        data,
        tdutils::promise::PromiseCreator::lambda(|()| ()),
    );
}

/// The `MessageContentStorage` bridge the database family stores messages
/// through — the file/animation/... pair ids round-trip the raw `FileId`
/// numbers like the M.9 `RawIdStorage` until the M.12.2 manager impls land.
pub struct MessageDbStorageBridge;

impl crate::message_content::MessageContentStorage for MessageDbStorageBridge {
    fn store_file(&self, file_id: FileId, out: &mut Vec<u8>) {
        store_i32(out, file_id.get());
    }

    fn parse_file(&mut self, parser: &mut Parser) -> Result<FileId, &'static str> {
        Ok(FileId::new(parser.i32_()?, 0))
    }

    fn store_animation(&self, file_id: FileId, out: &mut Vec<u8>) {
        self.store_file(file_id, out);
    }

    fn parse_animation(&mut self, parser: &mut Parser) -> Result<FileId, &'static str> {
        self.parse_file(parser)
    }

    fn store_audio(&self, file_id: FileId, out: &mut Vec<u8>) {
        self.store_file(file_id, out);
    }

    fn parse_audio(&mut self, parser: &mut Parser) -> Result<FileId, &'static str> {
        self.parse_file(parser)
    }

    fn store_document(&self, file_id: FileId, out: &mut Vec<u8>) {
        self.store_file(file_id, out);
    }

    fn parse_document(&mut self, parser: &mut Parser) -> Result<FileId, &'static str> {
        self.parse_file(parser)
    }

    fn store_video(&self, file_id: FileId, out: &mut Vec<u8>) {
        self.store_file(file_id, out);
    }

    fn parse_video(&mut self, parser: &mut Parser) -> Result<FileId, &'static str> {
        self.parse_file(parser)
    }

    fn store_video_note(&self, file_id: FileId, out: &mut Vec<u8>) {
        self.store_file(file_id, out);
    }

    fn parse_video_note(&mut self, parser: &mut Parser) -> Result<FileId, &'static str> {
        self.parse_file(parser)
    }

    fn store_voice_note(&self, file_id: FileId, out: &mut Vec<u8>) {
        self.store_file(file_id, out);
    }

    fn parse_voice_note(&mut self, parser: &mut Parser) -> Result<FileId, &'static str> {
        self.parse_file(parser)
    }

    fn store_sticker(&self, file_id: FileId, out: &mut Vec<u8>) {
        self.store_file(file_id, out);
    }

    fn parse_sticker(&mut self, parser: &mut Parser) -> Result<FileId, &'static str> {
        self.parse_file(parser)
    }

    fn store_poll(&self, poll_id: crate::message_content::PollId, out: &mut Vec<u8>) {
        store_i64(out, poll_id.get());
    }

    fn parse_poll(
        &mut self,
        parser: &mut Parser,
    ) -> Result<crate::message_content::PollId, &'static str> {
        Ok(crate::message_content::PollId::new(parser.i64_()?))
    }

    fn store_background(
        &self,
        background_id: crate::message_content::BackgroundId,
        out: &mut Vec<u8>,
    ) {
        store_i64(out, background_id.get());
    }

    fn parse_background(
        &mut self,
        parser: &mut Parser,
    ) -> Result<crate::message_content::BackgroundId, &'static str> {
        Ok(crate::message_content::BackgroundId::new(parser.i64_()?))
    }

    fn store_reactions(&mut self, reactions: &MessageReactions, out: &mut Vec<u8>) {
        reactions.store(out);
    }

    fn parse_reactions(&mut self, input: &mut Parser) -> Result<MessageReactions, &'static str> {
        MessageReactions::parse_from(input)
    }

    fn store_reply_markup(
        &mut self,
        reply_markup: &tdmtproto::telegram_api::ReplyMarkup,
        out: &mut Vec<u8>,
    ) {
        if let Some(markup) = ReplyMarkup::from_telegram(reply_markup.clone(), false, true, false) {
            markup.store(out);
        }
    }

    fn parse_reply_markup(
        &mut self,
        _input: &mut Parser,
    ) -> Result<tdmtproto::telegram_api::ReplyMarkup, &'static str> {
        // the input converter direction (internal -> telegram_api) lands
        // with the send path (M.10.8); the DB rows never need it before
        Err("ReplyMarkup wire conversion is not implemented yet")
    }

    fn store_min_channel(&mut self, min_channel: &MinChannel, out: &mut Vec<u8>) {
        store_min_channel(out, min_channel);
    }

    fn parse_min_channel(&mut self, input: &mut Parser) -> Result<MinChannel, &'static str> {
        parse_min_channel(input)
    }

    fn on_load_web_page_url_from_database(
        &mut self,
        _web_page_id: crate::message_content::WebPageId,
        _url: &str,
    ) {
    }
}

/// `MessagesManager::delete_all_dialog_messages_from_database`
/// (MessagesManager.cpp:31947-31977).
pub fn delete_all_dialog_messages_from_database(
    global: &crate::global::Global,
    dialog_id: DialogId,
    max_message_id: MessageId,
    source: &str,
) {
    debug_assert!(max_message_id.is_valid());
    if !global.use_message_database() {
        return;
    }

    tracing::info!(
        "Delete all messages in dialog {} from database up to message {} from {}",
        dialog_id.get(),
        max_message_id.get(),
        source
    );
    global.message_db_async().delete_all_dialog_messages(
        dialog_id,
        max_message_id,
        tdutils::promise::PromiseCreator::lambda(|()| ()),
    );
}

/// `MessagesManager::do_delete_message_log_event`
/// (MessagesManager.cpp:32154-32200) — the binlog-persisted delete + the
/// async row erase. The file-deletion leg of the C++ (FileManager promise
/// fan-in) lands with the Files slice; the log event and the row delete
/// are the durable half.
pub fn do_delete_message_log_event(
    global: &mut crate::global::Global,
    log_event: &DeleteMessageLogEvent,
) {
    debug_assert!(global.use_message_database());

    if !log_event.file_ids.is_empty() {
        // need_delete_file consults the file reference manager
        // ([blocked: M.12]); the log event is still written so the replay
        // sees the delete
        let mut data = Vec::new();
        log_event.store(&mut data);
        global
            .td_db()
            .binlog()
            .add(tddb::td_db::LOG_EVENT_HANDLER_TYPE_DELETE_MESSAGE, &data);
    }

    tracing::info!(
        "Delete message {}:{} from database",
        log_event.message_full_id.dialog_id.get(),
        log_event.message_full_id.message_id.get()
    );
    global.message_db_async().delete_message(
        log_event.message_full_id,
        tdutils::promise::PromiseCreator::lambda(|()| ()),
    );
}

/// `MessagesManager::delete_message_from_database`
/// (MessagesManager.cpp:32063-32152) — the database-relevant spine; the
/// notification-group and file-deletion side effects are seams until their
/// slices (M.13 wiring / M.12).
pub fn delete_message_from_database(
    global: &mut crate::global::Global,
    manager: &mut MessagesManager,
    dialog_id: DialogId,
    message_id: MessageId,
    m: Option<&Message>,
    is_permanently_deleted: bool,
    source: &str,
) {
    if !message_id.is_valid() && !message_id.is_valid_scheduled() {
        return;
    }
    if message_id.is_yet_unsent() {
        return;
    }

    // the local thread bookkeeping and the notification repairs are the
    // receive-path/files seams (recorded; M.10.3/M.13)

    if is_permanently_deleted {
        if message_id.is_scheduled()
            && message_id.is_valid_scheduled()
            && message_id.is_scheduled_server()
        {
            let scheduled = manager
                .dialogs
                .get_mut(&dialog_id)
                .and_then(|d| d.scheduled_messages.as_mut());
            if let Some(scheduled) = scheduled {
                scheduled
                    .deleted_scheduled_server_message_ids
                    .insert(message_id.get_scheduled_server_message_id());
            }
        } else if let Some(d) = manager.dialogs.get_mut(&dialog_id) {
            // don't store failed to send message identifiers for bots
            d.deleted_message_ids.insert(message_id);
        }

        if message_id.is_any_server()
            && let Some(old_message_id) = manager
                .update_message_ids
                .get(&(dialog_id, message_id))
                .copied()
            && old_message_id != MessageId::default()
        {
            tracing::warn!(
                "Sent message {}:{} was deleted before it was received",
                dialog_id.get(),
                message_id.get()
            );
            manager.update_message_ids.remove(&(dialog_id, message_id));
            // `delete_messages` fan-out lands with the delete path (M.10.6)
            let _ = old_message_id;
        }
    }

    if global.use_message_database() {
        let log_event = DeleteMessageLogEvent {
            id: 0,
            message_full_id: MessageFullId::new(dialog_id, message_id),
            file_ids: Vec::new(),
        };
        // the file-ids leg needs need_delete_message_files
        // ([blocked: M.12]); empty for now
        do_delete_message_log_event(global, &log_event);
        let _ = m;
        let _ = source;
    }
}

/// `MessagesManager::parse_message` (MessagesManager.cpp:19525-19620) —
/// the database-load validation entry. The bot/generation repairs ride the
/// Td context slices; the blob validation and the id matching are the
/// loadable core.
pub fn parse_message(
    time: &crate::user_manager::TimeCtx,
    expected_message_id: MessageId,
    value: &[u8],
    is_scheduled: bool,
) -> Option<Box<Message>> {
    let parse_result = parse_message_log_event(value, &mut MessageDbStorageBridge, time);
    let m = match parse_result {
        Ok(m) => m,
        Err(_) => return None,
    };
    let is_message_id_valid = if is_scheduled {
        if !expected_message_id.is_valid_scheduled() {
            false
        } else if m.message_id == expected_message_id {
            true
        } else {
            m.message_id.is_valid_scheduled()
                && expected_message_id.is_scheduled_server()
                && m.message_id.is_scheduled_server()
                && m.message_id.get_scheduled_server_message_id()
                    == expected_message_id.get_scheduled_server_message_id()
        }
    } else {
        expected_message_id.is_valid() && m.message_id == expected_message_id
    };
    if !is_message_id_valid {
        return None;
    }
    Some(Box::new(m))
}

/// `MessagesManager::DeleteMessageLogEvent` (MessagesManager.cpp:31979-32010).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DeleteMessageLogEvent {
    pub id: u64,
    pub message_full_id: MessageFullId,
    pub file_ids: Vec<FileId>,
}

impl DeleteMessageLogEvent {
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_file_ids = !self.file_ids.is_empty();
        store_flags(out, [has_file_ids]);
        self.message_full_id.store(out);
        if has_file_ids {
            store_vec_header(out, self.file_ids.len());
            for file_id in &self.file_ids {
                store_i32(out, file_id.get());
            }
        }
    }

    pub fn parse_from(parser: &mut Parser) -> Result<DeleteMessageLogEvent, &'static str> {
        let [has_file_ids] = parser.parse_flags::<1>()?;
        let message_full_id = MessageFullId::parse_from(parser)?;
        let file_ids = if has_file_ids {
            let size = parser.i32_()?;
            if size < 0 || size as usize > parser.left_len() {
                return Err("Wrong vector length");
            }
            let mut file_ids = Vec::with_capacity(size as usize);
            for _ in 0..size {
                file_ids.push(FileId::new(parser.i32_()?, 0));
            }
            file_ids
        } else {
            Vec::new()
        };
        Ok(DeleteMessageLogEvent {
            id: 0,
            message_full_id,
            file_ids,
        })
    }
}

/// `message_search_filter_count()` (MessageSearchFilter.h:43-45) — the
/// `Dialog` per-filter index/count arrays' length (`Size - 1`, i.e. every
/// real filter except `Empty`).
pub const MESSAGE_SEARCH_FILTER_COUNT: usize = 20;

/// `td::MessagesManager::Dialog` (MessagesManager.h:1308-1492) — the
/// per-dialog cache node. Field order and field-initializer defaults
/// follow the C++ struct (`order`/`pending_order = DEFAULT_ORDER` = -1,
/// `need_restore_reply_markup = true`); identity-typed like `Message`, so
/// no Clone/PartialEq. The `WaitFree*`/`Flat*` containers are std maps for
/// now; the intrusive node containers are stand-ins (see the section
/// notes).
#[derive(Debug)]
pub struct Dialog {
    pub dialog_id: DialogId,
    /// identifier of the last known server message received from update,
    /// there should be no server messages after it
    pub last_new_message_id: MessageId,
    /// identifier of the message after which currently there are no
    /// messages, i.e. a message without a gap after it, memory only
    pub last_message_id: MessageId,
    /// identifier of the first message in the database, needed until there
    /// are no gaps in the database
    pub first_database_message_id: MessageId,
    /// identifier of the last local or server message
    pub last_database_message_id: MessageId,
    pub first_database_message_id_by_index: [MessageId; MESSAGE_SEARCH_FILTER_COUNT],
    pub message_count_by_index: [i32; MESSAGE_SEARCH_FILTER_COUNT],
    pub server_unread_count: i32,
    pub local_unread_count: i32,
    pub unread_mention_count: i32,
    pub unread_reaction_count: i32,
    pub unread_poll_vote_count: i32,
    /// secret chats only
    pub last_read_inbox_message_date: i32,
    pub last_read_inbox_message_id: MessageId,
    pub last_read_outbox_message_id: MessageId,
    pub last_pinned_message_id: MessageId,
    pub reply_markup_message_id: MessageId,
    pub notification_settings: DialogNotificationSettings,
    pub available_reactions: ChatReactions,
    pub available_reactions_generation: u32,
    pub message_ttl: MessageTtl,
    pub draft_message: Option<DraftMessage>,
    pub action_bar: Option<DialogActionBar>,
    pub business_bot_manage_bar: Option<BusinessBotManageBar>,
    pub save_draft_message_log_event_id: LogEventIdWithGeneration,
    pub save_notification_settings_log_event_id: LogEventIdWithGeneration,
    pub set_folder_id_log_event_id: LogEventIdWithGeneration,
    pub active_group_call_id: InputGroupCallId,
    pub expected_active_group_call_id: InputGroupCallId,
    pub default_join_group_call_as_dialog_id: DialogId,
    pub default_send_message_as_dialog_id: DialogId,
    pub background_info: BackgroundInfo,
    pub chat_theme: Option<ChatTheme>,
    pub pending_join_request_count: i32,
    pub pending_join_request_user_ids: Vec<UserId>,
    pub have_full_history_source: i32,
    pub unload_dialog_delay_seed: i32,
    pub last_media_album_id: i64,
    pub history_generation: u32,
    pub open_count: u32,
    pub max_ephemeral_message_num: u32,
    pub notification_info: Option<NotificationInfo>,
    pub folder_id: FolderId,
    pub dialog_list_ids: Vec<DialogListId>,
    /// all mentions with a message identifier not greater than it are
    /// implicitly read
    pub last_read_all_mentions_message_id: MessageId,
    /// maximum unavailable message identifier for dialogs with
    /// cleared/unavailable history
    pub max_unavailable_message_id: MessageId,
    pub order: i64,
    pub pending_order: i64,
    pub deleted_last_message_id: MessageId,
    pub delete_last_message_date: i32,
    pub last_clear_history_date: i32,
    pub last_clear_history_message_id: MessageId,
    pub last_edited_message_id: MessageId,
    pub scheduled_messages_sync_generation: u32,
    pub last_repair_scheduled_messages_generation: u32,
    pub max_added_message_id: MessageId,
    pub being_added_message_id: MessageId,
    pub being_updated_last_new_message_id: MessageId,
    pub being_updated_last_database_message_id: MessageId,
    pub being_deleted_message_id: MessageId,
    pub has_contact_registered_message: bool,
    pub is_last_message_deleted_locally: bool,
    pub need_repair_action_bar: bool,
    pub know_action_bar: bool,
    pub has_outgoing_messages: bool,
    pub was_opened: bool,
    pub need_unload_on_close: bool,
    pub need_restore_reply_markup: bool,
    pub need_drop_default_send_message_as_dialog_id: bool,
    pub have_full_history: bool,
    pub is_empty: bool,
    pub is_last_read_inbox_message_id_inited: bool,
    pub is_last_read_outbox_message_id_inited: bool,
    pub is_last_pinned_message_id_inited: bool,
    pub is_folder_id_inited: bool,
    pub need_repair_server_unread_count: bool,
    pub need_repair_channel_server_unread_count: bool,
    pub is_forum: bool,
    pub is_forum_tabs: bool,
    pub view_as_messages: bool,
    pub is_marked_as_unread: bool,
    pub is_blocked: bool,
    pub is_is_blocked_inited: bool,
    pub is_blocked_for_stories: bool,
    pub is_is_blocked_for_stories_inited: bool,
    pub last_sent_has_scheduled_messages: bool,
    pub has_scheduled_server_messages: bool,
    pub has_scheduled_database_messages: bool,
    pub is_has_scheduled_database_messages_checked: bool,
    pub has_loaded_scheduled_messages_from_database: bool,
    pub sent_scheduled_messages: bool,
    pub has_welcome_messages: bool,
    /// whether the dialog was stored to database without last message
    pub had_last_yet_unsent_message: bool,
    pub has_active_group_call: bool,
    pub is_group_call_empty: bool,
    pub is_message_ttl_inited: bool,
    pub has_expected_active_group_call_id: bool,
    pub has_bots: bool,
    pub is_view_as_messages_inited: bool,
    pub is_saved_messages_view_as_messages_inited: bool,
    pub is_has_bots_inited: bool,
    pub is_background_inited: bool,
    pub is_chat_theme_inited: bool,
    pub is_available_reactions_inited: bool,
    pub had_yet_unsent_message_id_overflow: bool,
    pub need_repair_unread_mention_count: bool,
    pub need_repair_unread_reaction_count: bool,
    pub need_repair_unread_poll_vote_count: bool,
    pub is_translatable: bool,
    pub last_need_hide_dialog_draft_message: bool,
    pub last_need_hide_reactions: bool,
    pub is_update_new_chat_sent: bool,
    pub is_update_new_chat_being_sent: bool,
    pub has_unload_timeout: bool,
    pub is_channel_difference_finished: bool,
    /// for channels only
    pub pts: i32,
    /// for channels only
    pub pending_read_channel_inbox_pts: i32,
    /// for channels only
    pub pending_read_channel_inbox_server_unread_count: i32,
    /// for channels only
    pub pending_read_channel_inbox_max_message_id: MessageId,
    /// for secret chats and yet unsent messages only
    pub random_id_to_message_id: HashMap<i64, MessageId>,
    pub ephemeral_message_ids: HashMap<EphemeralMessageId, EphemeralMessageInfo>,
    /// identifier of the last local or yet unsent message, assigned after
    /// application start, used to guarantee that all assigned message
    /// identifiers are different
    pub last_assigned_message_id: MessageId,
    pub deleted_message_ids: HashSet<MessageId>,
    pub deleted_ephemeral_message_ids: HashSet<EphemeralMessageId>,
    pub client_data: String,
    pub messages: HashMap<MessageId, Box<Message>>,
    pub message_lru_list: MessageLruList,
    pub ordered_messages: OrderedMessages,
    pub scheduled_messages: Option<DialogScheduledMessages>,
    /// to be removed soon
    pub debug_set_dialog_last_database_message_id: &'static str,
    /// message identifiers loaded from database
    pub debug_last_new_message_id: MessageId,
    pub debug_first_database_message_id: MessageId,
    pub debug_last_database_message_id: MessageId,
}

/// `DEFAULT_ORDER` (DialogDate.h:69).
pub const DEFAULT_ORDER: i64 = -1;

impl Default for Dialog {
    /// The C++ field initializers (MessagesManager.h:1309-1466).
    fn default() -> Dialog {
        Dialog {
            dialog_id: DialogId::default(),
            last_new_message_id: MessageId::default(),
            last_message_id: MessageId::default(),
            first_database_message_id: MessageId::default(),
            last_database_message_id: MessageId::default(),
            first_database_message_id_by_index: [MessageId::default(); MESSAGE_SEARCH_FILTER_COUNT],
            message_count_by_index: [0; MESSAGE_SEARCH_FILTER_COUNT],
            server_unread_count: 0,
            local_unread_count: 0,
            unread_mention_count: 0,
            unread_reaction_count: 0,
            unread_poll_vote_count: 0,
            last_read_inbox_message_date: 0,
            last_read_inbox_message_id: MessageId::default(),
            last_read_outbox_message_id: MessageId::default(),
            last_pinned_message_id: MessageId::default(),
            reply_markup_message_id: MessageId::default(),
            notification_settings: DialogNotificationSettings::default(),
            available_reactions: ChatReactions::default(),
            available_reactions_generation: 0,
            message_ttl: MessageTtl::default(),
            draft_message: None,
            action_bar: None,
            business_bot_manage_bar: None,
            save_draft_message_log_event_id: LogEventIdWithGeneration::default(),
            save_notification_settings_log_event_id: LogEventIdWithGeneration::default(),
            set_folder_id_log_event_id: LogEventIdWithGeneration::default(),
            active_group_call_id: InputGroupCallId::default(),
            expected_active_group_call_id: InputGroupCallId::default(),
            default_join_group_call_as_dialog_id: DialogId::default(),
            default_send_message_as_dialog_id: DialogId::default(),
            background_info: BackgroundInfo::default(),
            chat_theme: None,
            pending_join_request_count: 0,
            pending_join_request_user_ids: Vec::new(),
            have_full_history_source: 0,
            unload_dialog_delay_seed: 0,
            last_media_album_id: 0,
            history_generation: 0,
            open_count: 0,
            max_ephemeral_message_num: 0,
            notification_info: None,
            folder_id: FolderId::default(),
            dialog_list_ids: Vec::new(),
            last_read_all_mentions_message_id: MessageId::default(),
            max_unavailable_message_id: MessageId::default(),
            order: DEFAULT_ORDER,
            pending_order: DEFAULT_ORDER,
            deleted_last_message_id: MessageId::default(),
            delete_last_message_date: 0,
            last_clear_history_date: 0,
            last_clear_history_message_id: MessageId::default(),
            last_edited_message_id: MessageId::default(),
            scheduled_messages_sync_generation: 0,
            last_repair_scheduled_messages_generation: 0,
            max_added_message_id: MessageId::default(),
            being_added_message_id: MessageId::default(),
            being_updated_last_new_message_id: MessageId::default(),
            being_updated_last_database_message_id: MessageId::default(),
            being_deleted_message_id: MessageId::default(),
            has_contact_registered_message: false,
            is_last_message_deleted_locally: false,
            need_repair_action_bar: false,
            know_action_bar: false,
            has_outgoing_messages: false,
            was_opened: false,
            need_unload_on_close: false,
            need_restore_reply_markup: true,
            need_drop_default_send_message_as_dialog_id: false,
            have_full_history: false,
            is_empty: false,
            is_last_read_inbox_message_id_inited: false,
            is_last_read_outbox_message_id_inited: false,
            is_last_pinned_message_id_inited: false,
            is_folder_id_inited: false,
            need_repair_server_unread_count: false,
            need_repair_channel_server_unread_count: false,
            is_forum: false,
            is_forum_tabs: false,
            view_as_messages: false,
            is_marked_as_unread: false,
            is_blocked: false,
            is_is_blocked_inited: false,
            is_blocked_for_stories: false,
            is_is_blocked_for_stories_inited: false,
            last_sent_has_scheduled_messages: false,
            has_scheduled_server_messages: false,
            has_scheduled_database_messages: false,
            is_has_scheduled_database_messages_checked: false,
            has_loaded_scheduled_messages_from_database: false,
            sent_scheduled_messages: false,
            has_welcome_messages: false,
            had_last_yet_unsent_message: false,
            has_active_group_call: false,
            is_group_call_empty: false,
            is_message_ttl_inited: false,
            has_expected_active_group_call_id: false,
            has_bots: false,
            is_view_as_messages_inited: false,
            is_saved_messages_view_as_messages_inited: false,
            is_has_bots_inited: false,
            is_background_inited: false,
            is_chat_theme_inited: false,
            is_available_reactions_inited: false,
            had_yet_unsent_message_id_overflow: false,
            need_repair_unread_mention_count: false,
            need_repair_unread_reaction_count: false,
            need_repair_unread_poll_vote_count: false,
            is_translatable: false,
            last_need_hide_dialog_draft_message: false,
            last_need_hide_reactions: false,
            is_update_new_chat_sent: false,
            is_update_new_chat_being_sent: false,
            has_unload_timeout: false,
            is_channel_difference_finished: false,
            pts: 0,
            pending_read_channel_inbox_pts: 0,
            pending_read_channel_inbox_server_unread_count: 0,
            pending_read_channel_inbox_max_message_id: MessageId::default(),
            random_id_to_message_id: HashMap::new(),
            ephemeral_message_ids: HashMap::new(),
            last_assigned_message_id: MessageId::default(),
            deleted_message_ids: HashSet::new(),
            deleted_ephemeral_message_ids: HashSet::new(),
            client_data: String::new(),
            messages: HashMap::new(),
            message_lru_list: MessageLruList::default(),
            ordered_messages: OrderedMessages::default(),
            scheduled_messages: None,
            debug_set_dialog_last_database_message_id: "Unknown",
            debug_last_new_message_id: MessageId::default(),
            debug_first_database_message_id: MessageId::default(),
            debug_last_database_message_id: MessageId::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn story_ids_semantics_and_roundtrip() {
        assert!(!StoryId::new(0).is_valid());
        assert!(StoryId::new(1).is_valid());
        assert!(StoryId::new(StoryId::MAX_SERVER_STORY_ID).is_server());
        assert!(!StoryId::new(StoryId::MAX_SERVER_STORY_ID + 1).is_server());

        let mut bytes = Vec::new();
        StoryId::new(7).store(&mut bytes);
        assert_eq!(StoryId::parse_value(&bytes).unwrap().get(), 7);

        assert_eq!(
            StoryId::get_story_ids(&StoryId::get_input_story_ids(&[
                StoryId::new(3),
                StoryId::new(9)
            ])),
            vec![StoryId::new(3), StoryId::new(9)]
        );
    }

    #[test]
    fn story_full_id_semantics_and_roundtrip() {
        let dialog_id = DialogId::from_user_id(UserId::new(123));
        assert!(!StoryFullId::default().is_valid());
        let full_id = StoryFullId::new(dialog_id, StoryId::new(5));
        assert!(full_id.is_valid());
        assert!(full_id.is_server());
        assert_eq!(full_id.get_dialog_id(), dialog_id);
        assert_eq!(full_id.get_story_id(), StoryId::new(5));

        let mut bytes = Vec::new();
        full_id.store(&mut bytes);
        assert_eq!(StoryFullId::parse_value(&bytes).unwrap(), full_id);

        // the hash is the C++ combine_hashes(DialogIdHash, StoryIdHash) form
        let hash_of = |id: StoryFullId| {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            std::hash::Hash::hash(&id, &mut hasher);
            std::hash::Hasher::finish(&hasher)
        };
        assert_eq!(
            hash_of(full_id),
            hash_of(StoryFullId::new(dialog_id, StoryId::new(5)))
        );
    }

    #[test]
    fn folder_id_clamps_to_main_and_archive() {
        assert_eq!(FolderId::new(0), FolderId::main());
        assert_eq!(FolderId::new(1), FolderId::archive());
        assert_eq!(FolderId::new(2), FolderId::main());
        assert_eq!(FolderId::new(-1), FolderId::main());

        let mut bytes = Vec::new();
        FolderId::archive().store(&mut bytes);
        assert_eq!(FolderId::parse_value(&bytes).unwrap(), FolderId::archive());

        // parse re-applies the clamp for out-of-band values
        let mut bad = Vec::new();
        store_i32(&mut bad, 7);
        assert_eq!(FolderId::parse_value(&bad).unwrap(), FolderId::main());
    }

    #[test]
    fn dialog_list_id_packs_folders_and_filters() {
        assert!(DialogListId::from_folder(FolderId::archive()).is_folder());
        assert_eq!(
            DialogListId::from_folder(FolderId::archive()).get_folder_id(),
            FolderId::archive()
        );
        assert!(!DialogListId::from_folder(FolderId::main()).is_filter());

        let filter_id = DialogFilterId::new(42);
        let list_id = DialogListId::from_filter(filter_id);
        assert!(list_id.is_filter());
        assert!(!list_id.is_folder());
        assert_eq!(list_id.get_filter_id(), filter_id);
        assert_eq!(DialogListId::FILTER_ID_SHIFT, 1i64 << 32);
    }

    #[test]
    fn dialog_filter_id_bounds() {
        assert!(!DialogFilterId::new(1).is_valid());
        assert!(DialogFilterId::new(DialogFilterId::MIN_FILTER_ID).is_valid());
        assert!(DialogFilterId::new(DialogFilterId::MAX_FILTER_ID).is_valid());

        let mut bytes = Vec::new();
        DialogFilterId::new(9).store(&mut bytes);
        assert_eq!(
            DialogFilterId::parse_value(&bytes).unwrap(),
            DialogFilterId::new(9)
        );
    }

    #[test]
    fn message_origin_inline_semantics() {
        assert!(MessageOrigin::default().is_empty());
        assert!(!MessageOrigin::default().is_channel_post());
        assert!(!MessageOrigin::default().has_sender_signature());

        let hidden = MessageOrigin::new(
            UserId::default(),
            DialogId::default(),
            MessageId::default(),
            String::new(),
            "Anonymous".to_string(),
        );
        assert!(!hidden.is_empty());
        assert!(hidden.has_sender_signature());
        assert_eq!(hidden.get_sender_name(), "Anonymous");

        let channel_post = MessageOrigin::new(
            UserId::default(),
            DialogId::default(),
            MessageId::from_server_message_id(ServerMessageId::new(11)),
            String::new(),
            String::new(),
        );
        assert!(channel_post.is_channel_post());
        assert!(!channel_post.has_sender_signature());
    }

    #[test]
    fn fact_check_semantics() {
        assert!(FactCheck::default().is_empty());
        assert!(
            !FactCheck {
                hash: 1,
                need_check: true,
                ..FactCheck::default()
            }
            .is_empty()
        );
        assert!(
            FactCheck {
                hash: 1,
                need_check: true,
                ..FactCheck::default()
            }
            .need_check()
        );
    }

    #[test]
    fn reaction_type_predicates() {
        assert!(ReactionType::default().is_empty());
        let emoji = ReactionType::new("👍");
        assert!(!emoji.is_empty());
        assert!(!emoji.is_custom_reaction());
        assert!(!emoji.is_paid_reaction());
        assert!(ReactionType::new("#12345").is_custom_reaction());
        assert!(ReactionType::new("$").is_paid_reaction());
    }

    #[test]
    fn chat_reactions_empty_rule() {
        assert!(ChatReactions::default().empty());
        assert!(!ChatReactions::legacy(vec![ReactionType::new("👍")]).empty());
        assert!(!ChatReactions::from_all_flags(true, false).empty());
        assert!(
            !ChatReactions {
                paid_reactions_available: true,
                ..ChatReactions::default()
            }
            .empty()
        );
        // allow_all_custom alone (without allow_all_regular) still counts as
        // empty by the C++ definition
        assert!(ChatReactions::from_all_flags(false, true).empty());
    }

    #[test]
    fn message_reply_info_defaults() {
        let info = MessageReplyInfo::default();
        assert!(info.is_empty());
        assert!(!info.was_dropped());
        assert_eq!(info.reply_count, -1);
        assert_eq!(info.pts, -1);
        assert_eq!(MessageReplyInfo::MAX_RECENT_REPLIERS, 3);

        let dropped = MessageReplyInfo {
            is_dropped: true,
            reply_count: 0,
            ..MessageReplyInfo::default()
        };
        assert!(!dropped.is_empty());
        assert!(dropped.was_dropped());
    }

    #[test]
    fn notification_sound_types() {
        assert_eq!(
            NotificationSound::default().get_type(),
            NotificationSoundType::None
        );
        assert_eq!(
            NotificationSound::Local("classic".to_string()).get_type(),
            NotificationSoundType::Local
        );
        assert_eq!(
            NotificationSound::Ringtone(77).get_type(),
            NotificationSoundType::Ringtone
        );
    }

    #[test]
    fn dialog_notification_settings_defaults() {
        let settings = DialogNotificationSettings::default();
        assert_eq!(settings.mute_until, 0);
        assert!(settings.sound.is_none());
        assert!(settings.story_sound.is_none());
        assert!(settings.show_preview);
        assert!(!settings.mute_stories);
        assert!(!settings.hide_story_sender);
        assert!(!settings.silent_send_message);
        assert!(settings.use_default_mute_until);
        assert!(settings.use_default_show_preview);
        assert!(settings.use_default_mute_stories);
        assert!(settings.use_default_hide_story_sender);
        assert!(!settings.is_synchronized);
        assert!(settings.use_default_disable_pinned_message_notifications);
        assert!(!settings.disable_pinned_message_notifications);
        assert!(settings.use_default_disable_mention_notifications);
        assert!(!settings.disable_mention_notifications);
    }

    #[test]
    fn dialog_action_bar_roundtrip() {
        let empty = DialogActionBar::default();
        let mut bytes = Vec::new();
        empty.store(&mut bytes);
        assert_eq!(DialogActionBar::parse_value(&bytes).unwrap(), empty);
        // the -1 distance default survives the roundtrip
        assert_eq!(DialogActionBar::parse_value(&bytes).unwrap().distance, -1);

        let full = DialogActionBar {
            distance: 42,
            join_request_date: 1700000000,
            join_request_dialog_title: "chat".to_string(),
            registration_month: 202401,
            phone_country: 972,
            last_name_change_date: 1700000001,
            last_photo_change_date: 1700000002,
            can_report_spam: true,
            can_add_contact: true,
            can_block_user: true,
            can_share_phone_number: true,
            can_report_location: true,
            can_unarchive: true,
            can_invite_members: true,
            is_join_request_broadcast: true,
        };
        bytes.clear();
        full.store(&mut bytes);
        assert_eq!(DialogActionBar::parse_value(&bytes).unwrap(), full);
        assert!(full.can_report_spam());
        assert!(full.can_unarchive());
    }

    #[test]
    fn business_bot_manage_bar_roundtrip() {
        let empty = BusinessBotManageBar::default();
        let mut bytes = Vec::new();
        empty.store(&mut bytes);
        assert_eq!(BusinessBotManageBar::parse_value(&bytes).unwrap(), empty);

        let full = BusinessBotManageBar {
            business_bot_user_id: UserId::new(12345),
            business_bot_manage_url: "https://t.me".to_string(),
            is_business_bot_paused: true,
            can_business_bot_reply: true,
        };
        bytes.clear();
        full.store(&mut bytes);
        assert_eq!(BusinessBotManageBar::parse_value(&bytes).unwrap(), full);

        // flags-only form: invalid user id and empty url are both dropped
        let flags_only = BusinessBotManageBar {
            is_business_bot_paused: true,
            ..BusinessBotManageBar::default()
        };
        bytes.clear();
        flags_only.store(&mut bytes);
        let parsed = BusinessBotManageBar::parse_value(&bytes).unwrap();
        assert!(parsed.is_business_bot_paused);
        assert!(!parsed.business_bot_user_id.is_valid());
        assert!(parsed.business_bot_manage_url.is_empty());
    }

    use super::*;

    // ---- ServerMessageId / ScheduledServerMessageId / EphemeralMessageId ----

    #[test]
    fn simple_ids_validity_and_roundtrip() {
        assert!(!ServerMessageId::new(0).is_valid());
        assert!(ServerMessageId::new(1).is_valid());

        assert!(!ScheduledServerMessageId::new(0).is_valid());
        assert!(ScheduledServerMessageId::new(1).is_valid());
        assert!(!ScheduledServerMessageId::new(1 << 18).is_valid());
        assert!(ScheduledServerMessageId::new((1 << 18) - 1).is_valid());

        assert!(!EphemeralMessageId::new(0).is_valid());
        assert!(EphemeralMessageId::new(3).is_valid());
        assert_eq!(
            EphemeralMessageId::get_ephemeral_message_ids(&[1, 7]),
            vec![EphemeralMessageId::new(1), EphemeralMessageId::new(7)]
        );

        let mut bytes = Vec::new();
        ScheduledServerMessageId::new(123).store(&mut bytes);
        assert_eq!(bytes, 123i32.to_le_bytes());
        assert_eq!(
            ScheduledServerMessageId::parse_value(&bytes).unwrap().get(),
            123
        );
        let mut bytes = Vec::new();
        EphemeralMessageId::new(456).store(&mut bytes);
        assert_eq!(bytes, 456i32.to_le_bytes());
        assert_eq!(EphemeralMessageId::parse_value(&bytes).unwrap().get(), 456);
    }

    // ---- MessageId ----

    #[test]
    fn message_id_layout() {
        assert_eq!(MessageId::min().get(), 1);
        assert_eq!(MessageId::max().get(), (i32::MAX as i64) << 20);

        let server = MessageId::from_server_message_id(ServerMessageId::new(1));
        assert_eq!(server.get(), 1 << 20);
        assert!(server.is_valid());
        assert!(server.is_server());
        assert!(server.is_any_server());
        assert_eq!(server.get_type(), MessageType::Server);
        assert_eq!(server.get_server_message_id().get(), 1);

        let yet_unsent = MessageId::new((1 << 20) | 1);
        assert_eq!(yet_unsent.get_type(), MessageType::YetUnsent);
        assert!(yet_unsent.is_valid());
        assert!(yet_unsent.is_yet_unsent());

        let local = MessageId::new((1 << 20) | 2);
        assert_eq!(local.get_type(), MessageType::Local);
        assert!(local.is_local());

        // type bits 3 in the low block are neither yet-unsent nor local
        assert!(!MessageId::new((1 << 20) | 3).is_valid());
        assert_eq!(MessageId::new((1 << 20) | 3).get_type(), MessageType::None);

        assert!(!MessageId::new(0).is_valid());
        assert!(!MessageId::new(-1).is_valid());
        assert_eq!(MessageId::default().get_type(), MessageType::None);
    }

    #[test]
    fn message_id_scheduled_layout() {
        let date = (1 << 30) + 5;
        let id = MessageId::from_scheduled(ScheduledServerMessageId::new(7), date, false);
        assert!(id.is_scheduled());
        assert!(id.is_valid_scheduled());
        assert!(id.is_scheduled_server());
        assert!(id.is_any_server());
        assert_eq!(id.get_type(), MessageType::Server);
        assert_eq!(id.get_scheduled_message_date(), date);
        assert_eq!(id.get_scheduled_server_message_id().get(), 7);

        // raw layout: (date - 2^30) << 21 | server << 3 | SCHEDULED_MASK
        assert_eq!(id.get(), (5i64 << 21) | (7 << 3) | 4);

        let scheduled_local = MessageId::new((5i64 << 21) | (7 << 3) | (4 | 2));
        assert_eq!(scheduled_local.get_type(), MessageType::Local);
        assert!(scheduled_local.is_local());
        let scheduled_yet_unsent = MessageId::new((5i64 << 21) | (7 << 3) | (4 | 1));
        assert_eq!(scheduled_yet_unsent.get_type(), MessageType::YetUnsent);

        // past dates and invalid ids log and fall back to the default
        assert_eq!(
            MessageId::from_scheduled(ScheduledServerMessageId::new(7), 1 << 30, false).get(),
            0
        );
        assert_eq!(
            MessageId::from_scheduled(ScheduledServerMessageId::new(0), date, false).get(),
            0
        );
        let forced = MessageId::from_scheduled(ScheduledServerMessageId::new(0), date, true);
        assert_eq!(forced.get(), (5i64 << 21) | 4);
        assert_eq!(forced.get_type(), MessageType::Server);

        assert!(!MessageId::new((1i64 << 51) | 4).is_valid_scheduled());
        assert!(!MessageId::new((5i64 << 21) | 3).is_valid_scheduled());
    }

    #[test]
    fn message_id_sponsored_band() {
        let sponsored = MessageId::new(MessageId::max().get() + 2);
        assert!(!sponsored.is_valid());
        assert!(sponsored.is_valid_sponsored());
        // the C++ get_type classifies the sponsored band as an ordinary local id
        assert_eq!(sponsored.get_type(), MessageType::Local);

        assert!(!MessageId::new(MessageId::max().get() + 1).is_valid_sponsored());
        assert!(!MessageId::new((1i64 << 51) + 2).is_valid_sponsored());
    }

    #[test]
    fn message_id_neighbor_conversions() {
        let local = MessageId::new((1 << 20) | 123);
        assert_eq!(local.get_prev_server_message_id().get(), 1 << 20);
        assert_eq!(local.get_next_server_message_id().get(), 2 << 20);

        let server = MessageId::from_server_message_id(ServerMessageId::new(1));
        let next_server = server.get_next_message_id(MessageType::Server);
        assert_eq!(next_server.get_server_message_id().get(), 2);
        assert_eq!(
            server.get_next_message_id(MessageType::YetUnsent).get(),
            (1 << 20) | 1
        );
        assert_eq!(
            server.get_next_message_id(MessageType::Local).get(),
            (1 << 20) | 2
        );

        let yet_unsent = MessageId::new((1 << 20) | 1);
        assert_eq!(
            yet_unsent.get_next_message_id(MessageType::Server).get(),
            2 << 20
        );
        assert_eq!(
            yet_unsent.get_next_message_id(MessageType::Local).get(),
            (1 << 20) | 2
        );
        let local = MessageId::new((1 << 20) | 2);
        assert_eq!(
            local.get_next_message_id(MessageType::YetUnsent).get(),
            (1 << 20) | 9
        );
        assert_eq!(
            local.get_next_message_id(MessageType::Local).get(),
            (1 << 20) | 10
        );

        // scheduled: promotion inside the block, then the next block
        let scheduled_server =
            MessageId::from_scheduled(ScheduledServerMessageId::new(7), (1 << 30) + 5, false);
        let promoted = scheduled_server.get_next_message_id(MessageType::YetUnsent);
        assert_eq!(promoted.get_type(), MessageType::YetUnsent);
        assert_eq!(promoted.get(), scheduled_server.get() + 1);
        let next_block = scheduled_server.get_next_message_id(MessageType::Server);
        assert!(next_block.is_scheduled_server());
        assert_eq!(next_block.get_scheduled_server_message_id().get(), 8);
        let next_block_local = scheduled_server.get_next_message_id(MessageType::Local);
        assert_eq!(next_block_local.get_type(), MessageType::Local);
        // Server(1) < Local(3): in-block promotion, not the next block
        assert_eq!(next_block_local.get(), scheduled_server.get() + 2);
        assert_eq!(next_block_local.get(), (5i64 << 21) | (7 << 3) | (4 | 2));
    }

    #[test]
    fn message_id_wire_helpers() {
        let date = (1 << 30) + 1000;
        let message = telegram_api::Message::message(telegram_api::message {
            id: 42,
            date,
            ..Default::default()
        });
        assert_eq!(
            MessageId::get_message_id(&message, false),
            MessageId::from_server_message_id(ServerMessageId::new(42))
        );
        let scheduled_wire = MessageId::get_message_id(&message, true);
        assert_eq!(scheduled_wire.get_scheduled_server_message_id().get(), 42);
        assert_eq!(scheduled_wire.get_scheduled_message_date(), date);

        let empty = telegram_api::Message::messageEmpty(telegram_api::messageEmpty {
            flags: 0,
            id: 9,
            peer_id: Default::default(),
        });
        assert_eq!(
            MessageId::get_message_id(&empty, false)
                .get_server_message_id()
                .get(),
            9
        );
        assert_eq!(MessageId::get_message_id(&empty, true).get(), 0);

        let service = telegram_api::Message::messageService(telegram_api::messageService {
            id: 5,
            date: (1 << 30) + 7,
            ..Default::default()
        });
        assert_eq!(
            MessageId::get_message_id(&service, false)
                .get_server_message_id()
                .get(),
            5
        );

        assert_eq!(MessageId::get_max_message_id(&[]), MessageId::default());
        assert_eq!(
            MessageId::get_max_message_id(&[
                telegram_api::Message::message(telegram_api::message {
                    id: 3,
                    date: 1000,
                    ..Default::default()
                }),
                telegram_api::Message::message(telegram_api::message {
                    id: 7,
                    date: 1000,
                    ..Default::default()
                }),
            ])
            .get_server_message_id()
            .get(),
            7
        );

        let ids = MessageId::get_message_ids(&[1 << 20, 2 << 20]);
        assert_eq!(MessageId::get_server_message_ids(&ids), vec![1, 2]);

        let scheduled_ids =
            MessageId::get_scheduled_server_message_ids(&[MessageId::from_scheduled(
                ScheduledServerMessageId::new(3),
                (1 << 30) + 1,
                false,
            )]);
        assert_eq!(scheduled_ids, vec![3]);

        let ok = [telegram_api::Message::message(telegram_api::message {
            id: 1,
            date: 1,
            ..Default::default()
        })];
        assert!(MessageId::is_message_id_order_ascending(&ok, "test"));
        assert!(MessageId::is_message_id_order_descending(&ok, "test"));
        assert!(!MessageId::is_message_id_order_ascending(
            &[
                telegram_api::Message::message(telegram_api::message {
                    id: 2,
                    date: 1,
                    ..Default::default()
                }),
                telegram_api::Message::message(telegram_api::message {
                    id: 1,
                    date: 1,
                    ..Default::default()
                }),
            ],
            "test"
        ));
        assert!(!MessageId::is_message_id_order_descending(
            &[
                telegram_api::Message::message(telegram_api::message {
                    id: 1,
                    date: 1,
                    ..Default::default()
                }),
                telegram_api::Message::message(telegram_api::message {
                    id: 2,
                    date: 1,
                    ..Default::default()
                }),
            ],
            "test"
        ));
    }

    #[test]
    fn message_id_display_and_ordering() {
        assert_eq!(
            format!(
                "{}",
                DisplayMessageId(MessageId::from_server_message_id(ServerMessageId::new(5)))
            ),
            "server message 5"
        );
        assert_eq!(
            format!("{}", DisplayMessageId(MessageId::new((1 << 20) | 10))),
            "local message 1.10"
        );
        assert_eq!(
            format!("{}", DisplayMessageId(MessageId::new((1 << 20) | 9))),
            "yet unsent message 1.9"
        );
        assert_eq!(
            format!("{}", DisplayMessageId(MessageId::new(-3))),
            "scheduled invalid message -3"
        );
        assert_eq!(
            format!("{}", DisplayMessageId(MessageId::new(-5))),
            "invalid message -5"
        );
        let scheduled =
            MessageId::from_scheduled(ScheduledServerMessageId::new(7), (1 << 30) + 5, false);
        assert_eq!(
            format!("{}", DisplayMessageId(scheduled)),
            "scheduled server message 7"
        );
        assert_eq!(
            format!(
                "{}",
                DisplayMessageId(MessageId::new((5i64 << 21) | (7 << 3) | 6))
            ),
            "scheduled local message 7"
        );
        assert_eq!(
            format!(
                "{}",
                DisplayMessageId(MessageId::new((5i64 << 21) | (7 << 3) | 5))
            ),
            "scheduled yet unsent message 7"
        );

        let a = MessageId::from_server_message_id(ServerMessageId::new(1));
        let b = MessageId::from_server_message_id(ServerMessageId::new(2));
        assert!(a < b);
        assert!(b > a);
        assert!(a <= a);

        let bytes = {
            let mut out = Vec::new();
            a.store(&mut out);
            out
        };
        assert_eq!(bytes, (1i64 << 20).to_le_bytes());
        assert_eq!(MessageId::parse_value(&bytes).unwrap(), a);
    }

    // ---- DialogId ----

    #[test]
    fn dialog_id_band_table() {
        assert_eq!(DialogId::new(0).get_type(), DialogType::None);
        assert_eq!(DialogId::new(1).get_type(), DialogType::User);
        assert_eq!(
            DialogId::new(UserId::MAX_USER_ID).get_type(),
            DialogType::User
        );
        assert_eq!(
            DialogId::new(UserId::MAX_USER_ID + 1).get_type(),
            DialogType::None
        );

        assert_eq!(DialogId::new(-1).get_type(), DialogType::Chat);
        assert_eq!(
            DialogId::new(-ChatId::MAX_CHAT_ID).get_type(),
            DialogType::Chat
        );
        // the C++ chain's sentinel exclusions fall through to the next band, so the
        // two zero sentinels (and the regular-channel lower edge) classify "wrongly"
        // — verified against a compiled C++ probe of the exact chain; pinned as-is
        assert_eq!(
            DialogId::new(DialogId::ZERO_CHANNEL_ID).get_type(),
            DialogType::SecretChat
        );

        assert_eq!(
            DialogId::new(DialogId::ZERO_CHANNEL_ID - 1).get_type(),
            DialogType::Channel
        );
        assert_eq!(
            DialogId::new(DialogId::ZERO_CHANNEL_ID - ChannelId::MAX_CHANNEL_ID).get_type(),
            DialogType::Channel
        );
        assert_eq!(
            DialogId::new(DialogId::ZERO_CHANNEL_ID - ChannelId::MAX_CHANNEL_ID - 1).get_type(),
            DialogType::SecretChat
        );

        assert_eq!(
            DialogId::new(DialogId::ZERO_SECRET_CHAT_ID).get_type(),
            DialogType::Channel
        );
        assert_eq!(
            DialogId::new(DialogId::ZERO_SECRET_CHAT_ID - 1).get_type(),
            DialogType::SecretChat
        );
        assert_eq!(
            DialogId::new(DialogId::ZERO_SECRET_CHAT_ID + i64::from(i32::MIN)).get_type(),
            DialogType::SecretChat
        );
        assert_eq!(
            DialogId::new(DialogId::ZERO_SECRET_CHAT_ID + i64::from(i32::MIN) - 1).get_type(),
            DialogType::Channel
        );
        assert_eq!(
            DialogId::new(DialogId::ZERO_CHANNEL_ID - ChannelId::MAX_MONOFORUM_CHANNEL_ID)
                .get_type(),
            DialogType::Channel
        );
        assert_eq!(
            DialogId::new(DialogId::ZERO_CHANNEL_ID - ChannelId::MAX_MONOFORUM_CHANNEL_ID - 1)
                .get_type(),
            DialogType::None
        );

        assert!(DialogId::new(1).is_valid());
        assert!(!DialogId::new(0).is_valid());
    }

    #[test]
    fn dialog_id_conversions() {
        let user = DialogId::from_user_id(UserId::new(7));
        assert_eq!(user.get(), 7);
        assert_eq!(user.get_type(), DialogType::User);
        assert_eq!(user.get_user_id().get(), 7);

        let chat = DialogId::from_chat_id(ChatId::new(9));
        assert_eq!(chat.get(), -9);
        assert_eq!(chat.get_chat_id().get(), 9);

        let channel = DialogId::from_channel_id(ChannelId::new(123));
        assert_eq!(channel.get(), DialogId::ZERO_CHANNEL_ID - 123);
        assert_eq!(channel.get_type(), DialogType::Channel);
        assert_eq!(channel.get_channel_id().get(), 123);

        let secret = DialogId::from_secret_chat_id(SecretChatId::new(55));
        assert_eq!(secret.get(), DialogId::ZERO_SECRET_CHAT_ID + 55);
        assert_eq!(secret.get_type(), DialogType::SecretChat);
        assert_eq!(secret.get_secret_chat_id().get(), 55);

        // invalid components decode to the default dialog, like the C++ ctors
        assert_eq!(DialogId::from_user_id(UserId::new(0)), DialogId::default());
        assert_eq!(DialogId::from_chat_id(ChatId::new(-1)), DialogId::default());
        assert_eq!(
            DialogId::from_channel_id(ChannelId::new(0)),
            DialogId::default()
        );
        assert_eq!(
            DialogId::from_secret_chat_id(SecretChatId::new(0)),
            DialogId::default()
        );
    }

    #[test]
    fn dialog_id_peers_and_messages() {
        let user = DialogId::from_user_id(UserId::new(7));
        let peer = telegram_api::Peer::peerUser(telegram_api::peerUser { user_id: 7 });
        assert_eq!(DialogId::from_peer(&peer), user);
        assert_eq!(
            DialogId::get_message_dialog_id(&telegram_api::Message::message(
                telegram_api::message {
                    id: 1,
                    date: 1,
                    peer_id: Some(peer.clone()),
                    ..Default::default()
                }
            )),
            user
        );

        let empty_no_peer = telegram_api::Message::messageEmpty(telegram_api::messageEmpty {
            flags: 0,
            id: 1,
            peer_id: Default::default(),
        });
        assert_eq!(
            DialogId::get_message_dialog_id(&empty_no_peer),
            DialogId::default()
        );
        let empty_with_peer = telegram_api::Message::messageEmpty(telegram_api::messageEmpty {
            flags: 1,
            id: 1,
            peer_id: Some(peer.clone()),
        });
        assert_eq!(DialogId::get_message_dialog_id(&empty_with_peer), user);

        let service = telegram_api::Message::messageService(telegram_api::messageService {
            id: 1,
            date: 1,
            peer_id: Some(peer),
            ..Default::default()
        });
        assert_eq!(DialogId::get_message_dialog_id(&service), user);

        let bad_peer = telegram_api::Peer::peerUser(telegram_api::peerUser { user_id: 0 });
        assert_eq!(DialogId::from_peer(&bad_peer), DialogId::default());

        let bytes = {
            let mut out = Vec::new();
            user.store(&mut out);
            out
        };
        assert_eq!(bytes, 7i64.to_le_bytes());
        assert_eq!(DialogId::parse_value(&bytes).unwrap(), user);
        assert_eq!(format!("{}", user), "chat 7");
    }

    #[test]
    fn message_full_id_roundtrip() {
        let full = MessageFullId::new(
            DialogId::from_user_id(UserId::new(7)),
            MessageId::from_server_message_id(ServerMessageId::new(42)),
        );
        assert_eq!(full.get_dialog_id(), DialogId::from_user_id(UserId::new(7)));
        assert_eq!(full.get_message_id().get_server_message_id().get(), 42);

        let mut out = Vec::new();
        full.store(&mut out);
        assert_eq!(out.len(), 16);
        assert_eq!(MessageFullId::parse_value(&out).unwrap(), full);

        let wire = telegram_api::Message::message(telegram_api::message {
            id: 42,
            date: 1000,
            peer_id: Some(telegram_api::Peer::peerUser(telegram_api::peerUser {
                user_id: 7,
            })),
            ..Default::default()
        });
        assert_eq!(
            MessageFullId::get_message_full_id(&wire, false),
            MessageFullId::new(
                DialogId::from_user_id(UserId::new(7)),
                MessageId::from_server_message_id(ServerMessageId::new(42))
            )
        );
    }

    // ---- Notification ids + NotificationGroupInfo ----

    #[test]
    fn notification_ids() {
        assert!(!NotificationGroupId::new(0).is_valid());
        assert!(NotificationGroupId::new(1).is_valid());
        assert_eq!(NotificationId::max().get(), i32::MAX);
        assert_eq!(NotificationObjectId::max().get(), MessageId::max().get());
        assert_eq!(
            NotificationObjectId::from_message_id(MessageId::from_server_message_id(
                ServerMessageId::new(9)
            ))
            .get(),
            9i64 << 20
        );
        assert!(NotificationObjectId::new(3) < NotificationObjectId::new(4));

        let key_recent =
            NotificationGroupKey::new(NotificationGroupId::new(1), DialogId::new(5), 100);
        let key_old = NotificationGroupKey::new(NotificationGroupId::new(2), DialogId::new(9), 50);
        assert!(key_recent < key_old, "newer date sorts first");
        let key_dialog =
            NotificationGroupKey::new(NotificationGroupId::new(1), DialogId::new(9), 100);
        assert!(
            key_dialog < key_recent,
            "bigger dialog id sorts first on equal date"
        );
        let key_group =
            NotificationGroupKey::new(NotificationGroupId::new(2), DialogId::new(9), 100);
        assert!(
            key_group < key_dialog,
            "bigger group id sorts first on full tie"
        );
    }

    #[test]
    fn notification_group_info_lifecycle() {
        let group_id = NotificationGroupId::new(3);
        let mut info = NotificationGroupInfo::with_group_id(group_id);
        assert!(info.is_valid());
        assert!(info.is_active());
        assert!(info.has_group_id(group_id));

        // key-change tracking
        let mut keys = Vec::new();
        info.add_group_key_if_changed(&mut keys, DialogId::new(10));
        assert_eq!(keys.len(), 1);
        assert_eq!(
            keys[0],
            NotificationGroupKey::new(group_id, DialogId::new(10), 0)
        );
        info.add_group_key_if_changed(&mut keys, DialogId::new(10));
        assert_eq!(keys.len(), 1, "no change, no key");

        assert!(info.set_last_notification(500, NotificationId::new(7)));
        assert!(
            !info.set_last_notification(500, NotificationId::new(7)),
            "same values, no change"
        );
        // the date change above is still pending — a repeat set does not consume it
        info.add_group_key_if_changed(&mut keys, DialogId::new(10));
        assert_eq!(keys.len(), 2);
        assert_eq!(
            keys[1],
            NotificationGroupKey::new(group_id, DialogId::new(10), 500)
        );
        // an id-only change emits nothing new (only date changes mark the key)
        assert!(info.set_last_notification(500, NotificationId::new(8)));
        info.add_group_key_if_changed(&mut keys, DialogId::new(10));
        assert_eq!(keys.len(), 2, "id-only change keeps the key clean");
        assert_eq!(info.get_last_notification_id(), NotificationId::new(8));

        // removed watermark filters late notifications
        assert!(!info.is_removed_notification_id(NotificationId::new(8)));
        assert!(
            info.set_max_removed_notification_id(
                NotificationId::new(8),
                NotificationObjectId::new(0)
            )
        );
        assert_eq!(
            info.get_last_notification_id(),
            NotificationId::default(),
            "last notification was removed"
        );
        assert_eq!(info.get_group_id(), group_id, "group id survives removals");

        // smaller watermarks are no-ops
        assert!(
            !info.set_max_removed_notification_id(
                NotificationId::new(7),
                NotificationObjectId::new(0)
            )
        );
        // larger watermarks bump the object watermark monotonically
        assert!(info.set_max_removed_notification_id(
            NotificationId::new(9),
            NotificationObjectId::new(1 << 20)
        ));
        assert!(info.is_removed_object_id(NotificationObjectId::new(1 << 20)));
        assert!(info.is_removed_notification(NotificationId::new(9), NotificationObjectId::new(2)));
        assert!(info.is_used_notification_id(NotificationId::new(9)));
        assert!(info.is_used_notification_id(NotificationId::new(8)));

        info.drop_max_removed_notification_id();
        assert!(!info.is_removed_notification_id(NotificationId::new(9)));
        // dropping again is a no-op
        info.drop_max_removed_notification_id();
    }

    #[test]
    fn notification_group_info_reuse() {
        let group_id = NotificationGroupId::new(5);
        let mut info = NotificationGroupInfo::with_group_id(group_id);

        // try_reuse needs a fully drained group
        info.try_reuse();
        assert!(!info.is_active());
        let mut keys = Vec::new();
        info.add_group_key_if_changed(&mut keys, DialogId::new(3));
        assert_eq!(
            keys[0].dialog_id,
            DialogId::default(),
            "reused groups lose the dialog"
        );

        assert_eq!(info.get_reused_group_id(), group_id);
        assert_eq!(info.get_group_id(), NotificationGroupId::default());
        assert!(!info.is_valid());
        assert!(!info.is_active());

        // a changed group cannot be reused: with_group_id starts key-changed and
        // try_reuse requires a fully drained group (last date 0, last id 0)
        let mut info = NotificationGroupInfo::with_group_id(group_id);
        info.try_reuse();
        assert_eq!(
            info.get_reused_group_id(),
            NotificationGroupId::default(),
            "changed group is not reused"
        );
        assert_eq!(
            info.get_group_id(),
            group_id,
            "failed reuse keeps the group"
        );
    }

    #[test]
    fn notification_group_info_store_parse() {
        let mut info = NotificationGroupInfo::with_group_id(NotificationGroupId::new(11));
        info.set_last_notification(700, NotificationId::new(21));
        info.set_max_removed_notification_id(
            NotificationId::new(20),
            NotificationObjectId::new(1 << 20),
        );

        let mut bytes = Vec::new();
        info.store(&mut bytes);
        let parsed = NotificationGroupInfo::parse_value(
            &bytes,
            ADD_NOTIFICATION_GROUP_INFO_MAX_REMOVED_MESSAGE_ID,
        )
        .unwrap();
        assert_eq!(parsed.get_group_id(), NotificationGroupId::new(11));
        assert_eq!(parsed.get_last_notification_id(), NotificationId::new(21));

        // pre-18 databases carry no object id field
        let mut old_bytes = Vec::new();
        NotificationGroupId::new(11).store(&mut old_bytes);
        store_i32(&mut old_bytes, 700);
        NotificationId::new(21).store(&mut old_bytes);
        NotificationId::new(20).store(&mut old_bytes);
        let old_parsed = NotificationGroupInfo::parse_value(
            &old_bytes,
            ADD_NOTIFICATION_GROUP_INFO_MAX_REMOVED_MESSAGE_ID - 1,
        )
        .unwrap();
        assert!(!old_parsed.is_removed_object_id(NotificationObjectId::new(1 << 20)));
        assert!(old_parsed.is_removed_notification_id(NotificationId::new(20)));
    }

    // ---- InputDialogId ----

    #[test]
    fn input_dialog_id_peers() {
        let user = InputDialogId::from_input_peer(&telegram_api::InputPeer::inputPeerUser(
            telegram_api::inputPeerUser {
                user_id: 7,
                access_hash: 99,
            },
        ));
        assert_eq!(user.get_dialog_id(), DialogId::from_user_id(UserId::new(7)));
        assert!(user.is_valid());
        let roundtrip = user.get_input_peer().unwrap();
        match roundtrip {
            telegram_api::InputPeer::inputPeerUser(p) => {
                assert_eq!(p.user_id, 7);
                assert_eq!(p.access_hash, 99);
            }
            _ => panic!("wrong peer type"),
        }

        let chat = InputDialogId::from_input_peer(&telegram_api::InputPeer::inputPeerChat(
            telegram_api::inputPeerChat { chat_id: 9 },
        ));
        assert!(chat.get_input_peer().is_some());

        let channel = InputDialogId::from_input_peer(&telegram_api::InputPeer::inputPeerChannel(
            telegram_api::inputPeerChannel {
                channel_id: 123,
                access_hash: -5,
            },
        ));
        assert_eq!(
            channel.get_dialog_id(),
            DialogId::from_channel_id(ChannelId::new(123))
        );
        match channel.get_input_peer().unwrap() {
            telegram_api::InputPeer::inputPeerChannel(p) => {
                assert_eq!(p.channel_id, 123);
                assert_eq!(p.access_hash, -5);
            }
            _ => panic!("wrong peer type"),
        }

        // secret chats and invalid dialogs have no input peer
        let secret = InputDialogId::new(DialogId::from_secret_chat_id(SecretChatId::new(4)));
        assert!(secret.get_input_peer().is_none());
        assert_eq!(
            InputDialogId::new(DialogId::default()).get_input_peer(),
            None
        );

        // invalid peers log and decode to the default
        let invalid = InputDialogId::from_input_peer(&telegram_api::InputPeer::inputPeerUser(
            telegram_api::inputPeerUser {
                user_id: 0,
                access_hash: 0,
            },
        ));
        assert_eq!(invalid, InputDialogId::default());
        let unsupported = InputDialogId::from_input_peer(&telegram_api::InputPeer::inputPeerSelf(
            telegram_api::inputPeerSelf {},
        ));
        assert_eq!(unsupported, InputDialogId::default());
    }

    #[test]
    fn input_dialog_id_collections() {
        let first = InputDialogId::from_input_peer(&telegram_api::InputPeer::inputPeerUser(
            telegram_api::inputPeerUser {
                user_id: 7,
                access_hash: 1,
            },
        ));
        let second = InputDialogId::from_input_peer(&telegram_api::InputPeer::inputPeerChat(
            telegram_api::inputPeerChat { chat_id: 9 },
        ));
        let secret = InputDialogId::new(DialogId::from_secret_chat_id(SecretChatId::new(4)));

        let mut added = Vec::new();
        let peers = vec![
            telegram_api::InputPeer::inputPeerUser(telegram_api::inputPeerUser {
                user_id: 7,
                access_hash: 1,
            }),
            telegram_api::InputPeer::inputPeerUser(telegram_api::inputPeerUser {
                user_id: 7,
                access_hash: 2,
            }),
            telegram_api::InputPeer::inputPeerChat(telegram_api::inputPeerChat { chat_id: 9 }),
        ];
        let ids = InputDialogId::get_input_dialog_ids(&peers, &mut added);
        assert_eq!(
            ids,
            vec![first, second],
            "duplicates are dropped by dialog id"
        );
        assert_eq!(added, vec![first.get_dialog_id(), second.get_dialog_id()]);

        assert_eq!(
            InputDialogId::get_dialog_ids(&[first, second]),
            vec![first.get_dialog_id(), second.get_dialog_id()]
        );
        assert!(InputDialogId::contains(
            &[first, second],
            first.get_dialog_id()
        ));
        assert!(!InputDialogId::contains(
            &[first, second],
            DialogId::default()
        ));

        let mut list = vec![first, secret, second];
        assert!(InputDialogId::remove(&mut list, second.get_dialog_id()));
        assert!(!InputDialogId::remove(&mut list, second.get_dialog_id()));
        assert_eq!(list, vec![first, secret]);

        assert!(InputDialogId::are_equivalent(
            &[first, secret, second],
            &[first, second]
        ));
        assert!(InputDialogId::are_equivalent(&[secret], &[]));
        assert!(!InputDialogId::are_equivalent(&[first], &[second]));

        let dialog_peers = InputDialogId::get_input_dialog_peers(&[first, secret, second]);
        assert_eq!(dialog_peers.len(), 2, "secret chats are skipped");
        let input_peers = InputDialogId::get_input_peers(&[first, second]);
        assert_eq!(input_peers.len(), 2);
    }

    #[test]
    fn input_dialog_id_store_parse() {
        let id = InputDialogId::with_access_hash(DialogId::from_user_id(UserId::new(7)), 12345);
        let mut bytes = Vec::new();
        id.store(&mut bytes);
        assert_eq!(bytes.len(), 16);
        assert_eq!(InputDialogId::parse_value(&bytes).unwrap(), id);
        assert_eq!(format!("{}", id), "input chat 7");
    }

    // ---- MessageContentType + MessageSelfDestructType ----

    #[test]
    fn message_content_type_discriminants() {
        assert_eq!(MessageContentType::None as i32, -1);
        assert_eq!(MessageContentType::Text as i32, 0);
        assert_eq!(MessageContentType::Photo as i32, 4);
        assert_eq!(MessageContentType::Unsupported as i32, 27);
        assert_eq!(MessageContentType::VideoNote as i32, 31);
        assert_eq!(MessageContentType::Poll as i32, 40);
        assert_eq!(MessageContentType::ChatJoinedViaCommunity as i32, 95);
        assert_eq!(MessageContentType::default(), MessageContentType::Text);
    }

    #[test]
    fn can_be_secret_message_content_table() {
        for ty in [
            MessageContentType::Animation,
            MessageContentType::Audio,
            MessageContentType::Photo,
            MessageContentType::Video,
            MessageContentType::VideoNote,
            MessageContentType::VoiceNote,
        ] {
            assert!(can_be_secret_message_content(ty), "{:?}", ty);
        }
        for ty in [
            MessageContentType::Text,
            MessageContentType::Document,
            MessageContentType::Sticker,
            MessageContentType::ExpiredPhoto,
            MessageContentType::StarGiftUnique,
            MessageContentType::ChatJoinedViaCommunity,
        ] {
            assert!(!can_be_secret_message_content(ty), "{:?}", ty);
        }
    }

    #[test]
    fn message_self_destruct_type_decisions() {
        // the forced replacement of a disallowed immediate
        assert_eq!(
            MessageSelfDestructType::with_ttl(0x7FFFFFFF, false).get_input_ttl(),
            0x7FFFFFFE
        );
        assert!(MessageSelfDestructType::with_ttl(0x7FFFFFFF, true).is_immediate());

        let ttl = MessageSelfDestructType::with_ttl(30, true);
        assert!(ttl.is_valid());
        assert!(!ttl.is_empty());
        assert!(ttl.is_secret_message_content(MessageContentType::Photo));
        assert!(!ttl.is_secret_message_content(MessageContentType::Text));
        // ttl above the 60-second band decays to the immediate check
        assert!(
            !MessageSelfDestructType::with_ttl(100, true)
                .is_secret_message_content(MessageContentType::Photo)
        );
        assert!(
            MessageSelfDestructType::with_ttl(0x7FFFFFFF, true)
                .is_secret_message_content(MessageContentType::Text),
            "immediate self-destructs regardless of the content type"
        );

        // ensure_at_least branches — a non-positive ttl_ is never raised
        let mut empty = MessageSelfDestructType::default();
        empty.ensure_at_least(40);
        assert_eq!(empty.get_input_ttl(), 0, "empty is never raised");
        let mut low = MessageSelfDestructType::with_ttl(30, true);
        low.ensure_at_least(40);
        assert_eq!(low.get_input_ttl(), 40);
        let mut high = MessageSelfDestructType::with_ttl(50, true);
        high.ensure_at_least(40);
        assert_eq!(high.get_input_ttl(), 50);
        let mut immediate = MessageSelfDestructType::with_ttl(0x7FFFFFFF, true);
        immediate.ensure_at_least(40);
        assert!(immediate.is_immediate(), "immediate is never raised");
        let mut zero = MessageSelfDestructType::default();
        zero.ensure_at_least(0);
        assert!(zero.is_empty(), "non-positive ttl is a no-op");
    }

    #[test]
    fn message_self_destruct_type_api_and_store() {
        assert_eq!(
            MessageSelfDestructType::get_message_self_destruct_type(None).unwrap(),
            MessageSelfDestructType::default()
        );
        let timer = MessageSelfDestructType::get_message_self_destruct_type(Some(
            td_api::MessageSelfDestructType::messageSelfDestructTypeTimer(
                td_api::messageSelfDestructTypeTimer {
                    self_destruct_time: 30,
                },
            ),
        ))
        .unwrap();
        assert_eq!(timer.get_input_ttl(), 30);
        assert_eq!(
            MessageSelfDestructType::get_message_self_destruct_type(Some(
                td_api::MessageSelfDestructType::messageSelfDestructTypeTimer(
                    td_api::messageSelfDestructTypeTimer {
                        self_destruct_time: 61,
                    }
                ),
            ))
            .unwrap_err()
            .code(),
            400
        );
        let immediate = MessageSelfDestructType::get_message_self_destruct_type(Some(
            td_api::MessageSelfDestructType::messageSelfDestructTypeImmediately(
                td_api::messageSelfDestructTypeImmediately,
            ),
        ))
        .unwrap();
        assert!(immediate.is_immediate());

        match timer.get_message_self_destruct_type_object().unwrap() {
            td_api::MessageSelfDestructType::messageSelfDestructTypeTimer(t) => {
                assert_eq!(t.self_destruct_time, 30);
            }
            _ => panic!("wrong self-destruct type"),
        }
        assert!(
            MessageSelfDestructType::default()
                .get_message_self_destruct_type_object()
                .is_none()
        );
        match immediate.get_message_self_destruct_type_object().unwrap() {
            td_api::MessageSelfDestructType::messageSelfDestructTypeImmediately(_) => (),
            _ => panic!("wrong self-destruct type"),
        }

        let mut bytes = Vec::new();
        timer.store(&mut bytes);
        assert_eq!(bytes, 30i32.to_le_bytes());
        assert_eq!(MessageSelfDestructType::parse_value(&bytes).unwrap(), timer);

        assert_eq!(format!("{}", timer), "self-destruct at 30");
        assert_eq!(format!("{}", immediate), "self-destruct immediately");
        assert_eq!(
            format!("{}", MessageSelfDestructType::default()),
            "non-self-destruct"
        );
    }

    // ---- MessageTtl / MessageEffectId / SavedMessagesTopicId / info structs ----

    #[test]
    fn message_ttl_basics() {
        assert!(MessageTtl::default().is_empty());
        let ttl = MessageTtl::new(86400);
        assert!(!ttl.is_empty());
        assert_eq!(ttl.get_message_auto_delete_time_object(), 86400);
        assert_eq!(ttl.get_input_ttl_period(), 86400);
        // negative periods log and clamp to zero, like the C++ constructor
        let negative = MessageTtl::new(-5);
        assert!(negative.is_empty());
        assert_eq!(negative.get_message_auto_delete_time_object(), 0);

        let mut bytes = Vec::new();
        ttl.store(&mut bytes);
        assert_eq!(MessageTtl::parse_value(&bytes).unwrap(), ttl);
        assert_eq!(format!("{}", ttl), "MessageTtl[86400]");
    }

    #[test]
    fn message_effect_id_basics() {
        assert!(!MessageEffectId::new(0).is_valid());
        assert!(MessageEffectId::new(7).is_valid());
        let id = MessageEffectId::new(123456);
        let mut bytes = Vec::new();
        id.store(&mut bytes);
        assert_eq!(bytes, 123456i64.to_le_bytes());
        assert_eq!(MessageEffectId::parse_value(&bytes).unwrap(), id);
        assert_eq!(format!("{}", id), "message effect 123456");
    }

    #[test]
    fn saved_messages_topic_id_basics() {
        let topic = SavedMessagesTopicId::new(DialogId::from_user_id(UserId::new(7)));
        assert!(topic.is_valid());
        assert_eq!(
            topic.get_dialog_id(),
            DialogId::from_user_id(UserId::new(7))
        );
        assert_eq!(topic.get_unique_id(), 7);
        assert!(!SavedMessagesTopicId::default().is_valid());

        let mut bytes = Vec::new();
        topic.store(&mut bytes);
        assert_eq!(bytes, 7i64.to_le_bytes());
        assert_eq!(SavedMessagesTopicId::parse_value(&bytes).unwrap(), topic);
    }

    #[test]
    fn notification_and_ephemeral_info_structs() {
        let mut info = NotificationInfo::default();
        assert!(!info.message_notification_group.is_valid());
        assert!(info.pending_new_message_notifications.is_empty());
        info.pending_new_message_notifications.push((
            DialogId::from_user_id(UserId::new(7)),
            MessageId::new(1 << 20),
        ));
        info.notification_id_to_message_id
            .insert(NotificationId::new(3), MessageId::new(1 << 20));
        assert_eq!(
            info.notification_id_to_message_id
                .get(&NotificationId::new(3)),
            Some(&MessageId::new(1 << 20))
        );

        let ephemeral = EphemeralMessageInfo {
            anchor_message_id: MessageId::new(1 << 20),
            monotonic_id: 9,
        };
        assert_eq!(ephemeral.monotonic_id, 9);
    }

    #[test]
    fn message_and_dialog_keep_the_cpp_field_defaults() {
        let message = Message::default();
        assert_eq!(message.max_reply_media_timestamp, -1);
        assert_eq!(message.max_own_media_timestamp, -2);
        assert_eq!(message.debug_source, "null");
        assert!(message.content.is_none());
        assert!(!message.is_outgoing);

        let dialog = Dialog::default();
        assert_eq!(dialog.order, DEFAULT_ORDER);
        assert_eq!(dialog.pending_order, DEFAULT_ORDER);
        assert!(dialog.need_restore_reply_markup);
        assert_eq!(dialog.debug_set_dialog_last_database_message_id, "Unknown");
        assert_eq!(
            dialog.message_count_by_index,
            [0; MESSAGE_SEARCH_FILTER_COUNT]
        );
        assert_eq!(MESSAGE_SEARCH_FILTER_COUNT, 20);
        assert!(dialog.messages.is_empty());
        assert!(
            dialog
                .first_database_message_id_by_index
                .iter()
                .all(|&id| id == MessageId::default())
        );
    }

    #[test]
    fn reply_forward_draft_plane_predicates() {
        let legacy = RepliedMessageInfo::legacy(
            MessageId::from_server_message_id(ServerMessageId::new(5)),
            DialogId::default(),
        );
        assert!(!legacy.is_empty());
        assert!(!legacy.is_external());
        assert!(RepliedMessageInfo::default().is_empty());

        let external = RepliedMessageInfo {
            origin_date: 1700000000,
            ..RepliedMessageInfo::default()
        };
        assert!(external.is_external());

        assert!(MessageQuote::default().is_empty());
        assert!(InputMessageText::default().is_empty());
        assert!(
            !InputMessageText {
                web_page_url: "https://t.me".to_string(),
                ..InputMessageText::default()
            }
            .is_empty()
        );

        let last_info = LastForwardedMessageInfo::new(
            DialogId::from_channel_id(ChannelId::new(123)),
            MessageId::from_server_message_id(ServerMessageId::new(7)),
            DialogId::default(),
            String::new(),
            1700000001,
            false,
        );
        assert!(!last_info.has_sender_name());
        assert_eq!(
            last_info.get_message_full_id(),
            MessageFullId::new(
                DialogId::from_channel_id(ChannelId::new(123)),
                MessageId::from_server_message_id(ServerMessageId::new(7))
            )
        );

        let voice = DraftMessageContent::VoiceNote(DraftMessageContentVoiceNote::default());
        assert_eq!(voice.get_type(), DraftMessageContentType::VoiceNote);

        let mut draft = DraftMessage {
            date: 1700000002,
            ..DraftMessage::default()
        };
        assert_eq!(draft.get_date(), 1700000002);
        assert!(!draft.is_local());
        draft.local_content = Some(DraftMessageContent::VideoNote(
            DraftMessageContentVideoNote::default(),
        ));
        assert!(draft.is_local());

        let pending = SuggestedPost::default();
        assert!(pending.is_pending());
        let accepted = SuggestedPost {
            is_accepted: true,
            ..pending
        };
        assert!(!accepted.is_pending());
    }

    #[test]
    fn dialog_scheduled_messages_container() {
        let mut scheduled = DialogScheduledMessages::default();
        let id = ScheduledServerMessageId::new(11);
        scheduled.scheduled_message_date.insert(id, 1700000003);
        scheduled
            .last_assigned_scheduled_message_id
            .insert(1700000003, MessageId::from_scheduled(id, 1700000003, false));
        scheduled.deleted_scheduled_server_message_ids.insert(id);
        assert_eq!(scheduled.scheduled_message_date.get(&id), Some(&1700000003));
        assert!(scheduled.deleted_scheduled_server_message_ids.contains(&id));
    }

    // ==== M.10.2a tests — the companion store/parse plane ====

    /// A `MessageContentStorage` that panics on any call: the companion
    /// tests store payloads without content, so the seam must never fire.
    struct NoStorage;

    impl crate::message_content::MessageContentStorage for NoStorage {
        fn store_file(&self, _file_id: FileId, _out: &mut Vec<u8>) {
            unreachable!()
        }
        fn parse_file(&mut self, _parser: &mut Parser) -> Result<FileId, &'static str> {
            unreachable!()
        }
        fn store_animation(&self, _file_id: FileId, _out: &mut Vec<u8>) {
            unreachable!()
        }
        fn parse_animation(&mut self, _parser: &mut Parser) -> Result<FileId, &'static str> {
            unreachable!()
        }
        fn store_audio(&self, _file_id: FileId, _out: &mut Vec<u8>) {
            unreachable!()
        }
        fn parse_audio(&mut self, _parser: &mut Parser) -> Result<FileId, &'static str> {
            unreachable!()
        }
        fn store_document(&self, _file_id: FileId, _out: &mut Vec<u8>) {
            unreachable!()
        }
        fn parse_document(&mut self, _parser: &mut Parser) -> Result<FileId, &'static str> {
            unreachable!()
        }
        fn store_video(&self, _file_id: FileId, _out: &mut Vec<u8>) {
            unreachable!()
        }
        fn parse_video(&mut self, _parser: &mut Parser) -> Result<FileId, &'static str> {
            unreachable!()
        }
        fn store_video_note(&self, _file_id: FileId, _out: &mut Vec<u8>) {
            unreachable!()
        }
        fn parse_video_note(&mut self, _parser: &mut Parser) -> Result<FileId, &'static str> {
            unreachable!()
        }
        fn store_voice_note(&self, _file_id: FileId, _out: &mut Vec<u8>) {
            unreachable!()
        }
        fn parse_voice_note(&mut self, _parser: &mut Parser) -> Result<FileId, &'static str> {
            unreachable!()
        }
        fn store_sticker(&self, _file_id: FileId, _out: &mut Vec<u8>) {
            unreachable!()
        }
        fn parse_sticker(&mut self, _parser: &mut Parser) -> Result<FileId, &'static str> {
            unreachable!()
        }
        fn store_poll(&self, _poll_id: crate::message_content::PollId, _out: &mut Vec<u8>) {
            unreachable!()
        }
        fn parse_poll(
            &mut self,
            _parser: &mut Parser,
        ) -> Result<crate::message_content::PollId, &'static str> {
            unreachable!()
        }
        fn store_background(
            &self,
            _background_id: crate::message_content::BackgroundId,
            _out: &mut Vec<u8>,
        ) {
            unreachable!()
        }
        fn parse_background(
            &mut self,
            _parser: &mut Parser,
        ) -> Result<crate::message_content::BackgroundId, &'static str> {
            unreachable!()
        }
        fn store_reactions(&mut self, _reactions: &MessageReactions, _out: &mut Vec<u8>) {
            unreachable!()
        }
        fn parse_reactions(
            &mut self,
            _input: &mut Parser,
        ) -> Result<MessageReactions, &'static str> {
            unreachable!()
        }
        fn store_reply_markup(
            &mut self,
            _reply_markup: &tdmtproto::telegram_api::ReplyMarkup,
            _out: &mut Vec<u8>,
        ) {
            unreachable!()
        }
        fn parse_reply_markup(
            &mut self,
            _input: &mut Parser,
        ) -> Result<tdmtproto::telegram_api::ReplyMarkup, &'static str> {
            unreachable!()
        }
        fn store_min_channel(&mut self, _min_channel: &MinChannel, _out: &mut Vec<u8>) {
            unreachable!()
        }
        fn parse_min_channel(&mut self, _input: &mut Parser) -> Result<MinChannel, &'static str> {
            unreachable!()
        }
        fn on_load_web_page_url_from_database(
            &mut self,
            _web_page_id: crate::message_content::WebPageId,
            _url: &str,
        ) {
            unreachable!()
        }
    }

    #[test]
    fn message_origin_store_parse_roundtrip() {
        let empty = MessageOrigin::default();
        let mut bytes = Vec::new();
        empty.store(&mut bytes);
        // the empty origin is exactly the clear flag word
        assert_eq!(bytes, vec![0, 0, 0, 0]);
        let mut parser = Parser::new(&bytes);
        assert_eq!(MessageOrigin::parse_from(&mut parser).unwrap(), empty);

        let full = MessageOrigin::new(
            UserId::new(7),
            DialogId::from_channel_id(ChannelId::new(10)),
            MessageId::from_server_message_id(ServerMessageId::new(100)),
            "sig".to_string(),
            "name".to_string(),
        );
        let mut bytes = Vec::new();
        full.store(&mut bytes);
        let mut parser = Parser::new(&bytes);
        assert_eq!(MessageOrigin::parse_from(&mut parser).unwrap(), full);
    }

    #[test]
    fn forward_info_store_parse_roundtrip() {
        let mut info = MessageForwardInfo {
            origin: MessageOrigin::new(
                UserId::default(),
                DialogId::from_channel_id(ChannelId::new(42)),
                MessageId::from_server_message_id(ServerMessageId::new(500)),
                String::new(),
                String::new(),
            ),
            date: 1700000000,
            last_message_info: LastForwardedMessageInfo {
                dialog_id: DialogId::from_user_id(UserId::new(9)),
                message_id: MessageId::from_server_message_id(ServerMessageId::new(501)),
                sender_dialog_id: DialogId::from_user_id(UserId::new(9)),
                sender_name: "sender".to_string(),
                date: 1700000100,
                is_outgoing: true,
            },
            psa_type: "PSA".to_string(),
            is_imported: true,
        };
        let mut bytes = Vec::new();
        info.store(&mut bytes);
        let mut parser = Parser::new(&bytes);
        assert_eq!(MessageForwardInfo::parse_from(&mut parser).unwrap(), info);

        // validate() collapses an inconsistent last_message_info (sender
        // name without a date)
        info.last_message_info.date = 0;
        let mut bytes = Vec::new();
        info.store(&mut bytes);
        let mut parser = Parser::new(&bytes);
        let parsed = MessageForwardInfo::parse_from(&mut parser).unwrap();
        assert!(parsed.last_message_info.is_empty());
    }

    #[test]
    fn message_reply_info_store_parse_roundtrip() {
        let mut info = MessageReplyInfo {
            reply_count: 3,
            pts: 100,
            recent_replier_dialog_ids: vec![
                DialogId::from_user_id(UserId::new(1)),
                DialogId::from_user_id(UserId::new(2)),
            ],
            replier_min_channels: vec![(
                ChannelId::new(77),
                MinChannel {
                    title: "channel".to_string(),
                    is_megagroup: true,
                },
            )],
            channel_id: ChannelId::new(5),
            max_message_id: MessageId::from_server_message_id(ServerMessageId::new(600)),
            last_read_inbox_message_id: MessageId::from_server_message_id(ServerMessageId::new(
                599,
            )),
            last_read_outbox_message_id: MessageId::from_server_message_id(ServerMessageId::new(
                598,
            )),
            is_comment: true,
            is_dropped: false,
        };
        let mut bytes = Vec::new();
        info.store(&mut bytes);
        let mut parser = Parser::new(&bytes);
        assert_eq!(MessageReplyInfo::parse_from(&mut parser).unwrap(), info);

        // channel id 777 collapses to the dropped default
        info.channel_id = ChannelId::new(777);
        let mut bytes = Vec::new();
        info.store(&mut bytes);
        let mut parser = Parser::new(&bytes);
        let parsed = MessageReplyInfo::parse_from(&mut parser).unwrap();
        assert!(parsed.is_dropped);
        assert!(parsed.is_empty());
    }

    #[test]
    fn replied_message_info_store_parse_roundtrip() {
        let info = RepliedMessageInfo {
            message_id: MessageId::from_server_message_id(ServerMessageId::new(700)),
            dialog_id: DialogId::from_user_id(UserId::new(3)),
            origin_date: 1700000200,
            origin: MessageOrigin::new(
                UserId::new(4),
                DialogId::default(),
                MessageId::default(),
                String::new(),
                "op".to_string(),
            ),
            content: None,
            quote: MessageQuote {
                text: FormattedText {
                    text: "quote".to_string(),
                    entities: Vec::new(),
                },
                position: 3,
                is_manual: true,
            },
            todo_item_id: 0,
            poll_option_id: String::new(),
        };
        let mut bytes = Vec::new();
        info.store(&NoStorage, &mut bytes);
        let mut parser = Parser::new(&bytes);
        assert_eq!(
            RepliedMessageInfo::parse_from(&mut parser, &mut NoStorage).unwrap(),
            info
        );
    }

    #[test]
    fn fact_check_store_parse_roundtrip() {
        let check = FactCheck {
            country_code: "US".to_string(),
            text: FormattedText {
                text: "text".to_string(),
                entities: Vec::new(),
            },
            hash: 123456789,
            need_check: false,
        };
        let mut bytes = Vec::new();
        check.store(&mut bytes);
        let mut parser = Parser::new(&bytes);
        assert_eq!(FactCheck::parse_from(&mut parser).unwrap(), check);

        // an empty fact check (hash 0) is rejected by parse
        let mut bytes = Vec::new();
        FactCheck::default().store(&mut bytes);
        let mut parser = Parser::new(&bytes);
        assert!(FactCheck::parse_from(&mut parser).is_err());
    }

    #[test]
    fn suggested_post_store_parse_roundtrip() {
        let post = SuggestedPost {
            price: SuggestedPostPrice {
                r#type: crate::message_content::SuggestedPostPriceType::Star,
                amount: 42,
            },
            schedule_date: 1700000300,
            is_accepted: false,
            is_rejected: false,
        };
        let mut bytes = Vec::new();
        post.store(&mut bytes);
        let mut parser = Parser::new(&bytes);
        assert_eq!(SuggestedPost::parse_from(&mut parser).unwrap(), post);
    }

    #[test]
    fn input_message_text_store_parse_roundtrip() {
        let text = InputMessageText {
            text: FormattedText {
                text: "hello".to_string(),
                entities: Vec::new(),
            },
            web_page_url: "https://example.com".to_string(),
            disable_web_page_preview: true,
            force_small_media: false,
            force_large_media: true,
            show_above_text: false,
            clear_draft: false,
        };
        let mut bytes = Vec::new();
        text.store(&mut bytes);
        let mut parser = Parser::new(&bytes);
        assert_eq!(InputMessageText::parse_from(&mut parser).unwrap(), text);

        // the empty text arm stores no FormattedText and no url at all
        let mut bytes = Vec::new();
        InputMessageText::default().store(&mut bytes);
        assert_eq!(bytes.len(), 4); // the flag word alone
    }

    #[test]
    fn draft_message_store_parse_roundtrip() {
        let draft = DraftMessage {
            date: 1700000400,
            message_input_reply_to: MessageInputReplyTo::regular(
                MessageId::from_server_message_id(ServerMessageId::new(800)),
            ),
            input_message_text: InputMessageText {
                text: FormattedText {
                    text: "draft".to_string(),
                    entities: Vec::new(),
                },
                web_page_url: String::new(),
                disable_web_page_preview: false,
                force_small_media: false,
                force_large_media: false,
                show_above_text: false,
                clear_draft: false,
            },
            rich_message_content: None,
            local_content: Some(DraftMessageContent::VoiceNote(
                DraftMessageContentVoiceNote {
                    path: "/tmp/voice.ogg".to_string(),
                    duration: 12,
                    waveform: "wave".to_string(),
                    ttl: MessageSelfDestructType::default(),
                },
            )),
            message_effect_id: MessageEffectId::new(50),
            suggested_post: None,
        };
        let mut bytes = Vec::new();
        draft.store(&NoStorage, &mut bytes);
        let mut parser = Parser::new(&bytes);
        let parsed = DraftMessage::parse_from(&mut parser, &mut NoStorage).unwrap();
        // parsing marks the reply source like the C++ `debug_source_ =
        // "binlog"`; everything else round-trips
        assert_eq!(parsed.message_input_reply_to.debug_source, "binlog");
        let mut parsed = parsed;
        parsed.message_input_reply_to.debug_source = draft.message_input_reply_to.debug_source;
        assert_eq!(parsed, draft);
    }

    #[test]
    fn draft_message_legacy_layout_reads_the_inline_reply_id() {
        // pre-SupportRepliesInOtherChats (51): flags absent, the reply
        // message id follows the date directly
        let mut bytes = Vec::new();
        store_i32(&mut bytes, 1700000500); // date
        MessageId::from_server_message_id(ServerMessageId::new(801)).store(&mut bytes);
        InputMessageText::default().store(&mut bytes);
        let mut parser = Parser::new_with_version(&bytes, 50);
        let parsed = DraftMessage::parse_from(&mut parser, &mut NoStorage).unwrap();
        assert_eq!(
            parsed.message_input_reply_to.message_id,
            MessageId::from_server_message_id(ServerMessageId::new(801))
        );
        assert_eq!(parsed.date, 1700000500);
    }

    #[test]
    fn message_input_reply_to_store_parse_roundtrip() {
        let full = MessageInputReplyTo {
            message_id: MessageId::from_server_message_id(ServerMessageId::new(900)),
            ephemeral_message_id: EphemeralMessageId::default(),
            dialog_id: DialogId::from_user_id(UserId::new(11)),
            quote: MessageQuote {
                text: FormattedText {
                    text: "q".to_string(),
                    entities: Vec::new(),
                },
                position: 0,
                is_manual: false,
            },
            todo_item_id: 2,
            poll_option_id: "1".to_string(),
            debug_source: "test",
            story_full_id: StoryFullId::new(
                DialogId::from_user_id(UserId::new(12)),
                StoryId::new(13),
            ),
        };
        let mut bytes = Vec::new();
        full.store(&mut bytes);
        let mut parser = Parser::new(&bytes);
        let parsed = MessageInputReplyTo::parse_from(&mut parser).unwrap();
        assert_eq!(parsed.message_id, full.message_id);
        assert_eq!(parsed.dialog_id, full.dialog_id);
        assert_eq!(parsed.quote, full.quote);
        assert_eq!(parsed.todo_item_id, full.todo_item_id);
        assert_eq!(parsed.story_full_id, full.story_full_id);
        assert_eq!(parsed.debug_source, "binlog");

        assert!(MessageInputReplyTo::regular(MessageId::default()).is_empty());
        assert!(
            !MessageInputReplyTo::regular(MessageId::from_server_message_id(ServerMessageId::new(
                1
            )))
            .is_empty()
        );
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn message_store_parse_roundtrip_full() {
        let time = crate::user_manager::TimeCtx {
            now: 1000.0,
            server_time: 1700000000.5,
        };
        let mut m = Message::default();
        m.message_id = MessageId::from_server_message_id(ServerMessageId::new(1234));
        m.sender_user_id = UserId::new(10);
        m.date = 1700000100;
        m.edit_date = 1700000200;
        m.random_id = 777;
        m.forward_info = Some(MessageForwardInfo {
            origin: MessageOrigin::new(
                UserId::default(),
                DialogId::from_channel_id(ChannelId::new(30)),
                MessageId::from_server_message_id(ServerMessageId::new(4321)),
                String::new(),
                String::new(),
            ),
            date: 1699990000,
            ..MessageForwardInfo::default()
        });
        m.via_bot_user_id = UserId::new(11);
        m.view_count = 100;
        m.reply_info = MessageReplyInfo {
            reply_count: 5,
            ..MessageReplyInfo::default()
        };
        m.ttl = MessageSelfDestructType::with_ttl(60, false);
        m.ttl_expires_at = 2000.0;
        m.author_signature = "sig".to_string();
        m.media_album_id = 42;
        m.notification_id = NotificationId::new(55);
        m.legacy_layer = 62;
        m.restriction_reasons = vec![RestrictionReason::new(
            "platform".to_string(),
            "spam".to_string(),
            "text".to_string(),
        )];
        m.sender_dialog_id = DialogId::from_channel_id(ChannelId::new(31));
        m.top_thread_message_id = MessageId::from_server_message_id(ServerMessageId::new(1233));
        m.linked_top_thread_message_id =
            MessageId::from_server_message_id(ServerMessageId::new(900));
        m.interaction_info_update_date = 1700000300;
        m.send_emoji = "🎲".to_string();
        m.content = Some(crate::message_content::MessageContent::Text(
            crate::message_content::MessageText {
                text: FormattedText {
                    text: "hello world".to_string(),
                    entities: Vec::new(),
                },
                ..crate::message_content::MessageText::default()
            },
        ));
        m.reply_markup = Some(Box::new(ReplyMarkup {
            r#type: ReplyMarkupType::InlineKeyboard,
            force_reply: true,
            inline_keyboard: vec![vec![InlineKeyboardButton {
                r#type: InlineKeyboardButtonType::Callback,
                data: "payload".to_string(),
                text: "click".to_string(),
                ..InlineKeyboardButton::default()
            }]],
            ..ReplyMarkup::default()
        }));
        m.ttl_period = 86400;
        m.max_reply_media_timestamp = 5;
        m.reactions = Some(MessageReactions {
            reactions: vec![MessageReaction {
                reaction_type: ReactionType::new("👍"),
                choose_count: 3,
                ..MessageReaction::default()
            }],
            ..MessageReactions::default()
        });
        m.available_reactions_generation = 7;
        m.history_generation = 2;
        m.reply_to_story_full_id =
            StoryFullId::new(DialogId::from_user_id(UserId::new(12)), StoryId::new(13));
        m.replied_message_info = RepliedMessageInfo {
            message_id: MessageId::from_server_message_id(ServerMessageId::new(1230)),
            ..RepliedMessageInfo::default()
        };
        m.saved_messages_topic_id =
            SavedMessagesTopicId::new(DialogId::from_user_id(UserId::new(14)));
        m.sender_boost_count = 2;
        m.via_business_bot_user_id = UserId::new(15);
        m.effect_id = MessageEffectId::new(64);
        m.fact_check = Some(FactCheck {
            country_code: "US".to_string(),
            hash: 999,
            ..FactCheck::default()
        });
        m.is_pinned = true;
        m.noforwards = true;
        m.contains_mention = true;

        let mut bytes = Vec::new();
        store_message_log_event(&m, &NoStorage, &time, &mut bytes);
        let parsed = parse_message_log_event(&bytes, &mut NoStorage, &time).unwrap();

        // identity-lite assertions over the parsed copy
        assert_eq!(parsed.message_id, m.message_id);
        assert_eq!(parsed.sender_user_id, m.sender_user_id);
        assert_eq!(parsed.date, m.date);
        assert_eq!(parsed.edit_date, m.edit_date);
        assert_eq!(parsed.random_id, m.random_id);
        assert_eq!(parsed.forward_info, m.forward_info);
        assert_eq!(parsed.via_bot_user_id, m.via_bot_user_id);
        assert_eq!(parsed.view_count, m.view_count);
        assert_eq!(parsed.reply_info, m.reply_info);
        assert_eq!(parsed.ttl, m.ttl);
        // store_time round-trips relative to the SAME clocks
        assert_eq!(parsed.ttl_expires_at, m.ttl_expires_at);
        assert_eq!(parsed.author_signature, m.author_signature);
        assert_eq!(parsed.media_album_id, m.media_album_id);
        assert_eq!(parsed.notification_id, m.notification_id);
        assert_eq!(parsed.legacy_layer, m.legacy_layer);
        assert_eq!(parsed.restriction_reasons, m.restriction_reasons);
        assert_eq!(parsed.sender_dialog_id, m.sender_dialog_id);
        assert_eq!(parsed.top_thread_message_id, m.top_thread_message_id);
        assert_eq!(
            parsed.linked_top_thread_message_id,
            m.linked_top_thread_message_id
        );
        assert_eq!(
            parsed.interaction_info_update_date,
            m.interaction_info_update_date
        );
        assert_eq!(parsed.send_emoji, m.send_emoji);
        assert_eq!(parsed.reply_markup, m.reply_markup);
        assert_eq!(parsed.ttl_period, m.ttl_period);
        assert_eq!(
            parsed.max_reply_media_timestamp,
            m.max_reply_media_timestamp
        );
        assert_eq!(parsed.reactions, m.reactions);
        assert_eq!(
            parsed.available_reactions_generation,
            m.available_reactions_generation
        );
        assert_eq!(parsed.history_generation, m.history_generation);
        assert_eq!(parsed.reply_to_story_full_id, m.reply_to_story_full_id);
        assert_eq!(parsed.replied_message_info, m.replied_message_info);
        assert_eq!(parsed.saved_messages_topic_id, m.saved_messages_topic_id);
        assert_eq!(parsed.sender_boost_count, m.sender_boost_count);
        assert_eq!(parsed.via_business_bot_user_id, m.via_business_bot_user_id);
        assert_eq!(parsed.effect_id, m.effect_id);
        assert_eq!(parsed.fact_check, m.fact_check);
        assert!(parsed.is_pinned);
        assert!(parsed.noforwards);
        assert!(parsed.contains_mention);
        match parsed.content.as_ref().unwrap() {
            crate::message_content::MessageContent::Text(text) => {
                assert_eq!(text.text.text, "hello world");
            }
            _ => panic!("wrong content"),
        }
        // and the re-store is byte-identical
        let mut bytes2 = Vec::new();
        store_message_log_event(&parsed, &NoStorage, &time, &mut bytes2);
        assert_eq!(bytes, bytes2);
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn message_store_minimal_blob_prefix_is_hand_computed() {
        // the emptiest message stores: the log-event version word, then the
        // four flag words — all clear except has_flags2/3/4 in the last bit
        // of their words (bit 29 / bit 29 / all-clear 13-bit word) — then
        // message_id, date and the content payload. The flag prefix is
        // pinned by hand; the tail rides the verified primitive stores.
        let time = crate::user_manager::TimeCtx {
            now: 0.0,
            server_time: 0.0,
        };
        let mut m = Message::default();
        m.message_id = MessageId::from_server_message_id(ServerMessageId::new(1));
        m.date = 100;
        m.content = Some(crate::message_content::MessageContent::Text(
            crate::message_content::MessageText::default(),
        ));

        let mut bytes = Vec::new();
        store_message_log_event(&m, &NoStorage, &time, &mut bytes);
        assert!(bytes.len() > 4 + 4 * 3);
        assert_eq!(&bytes[0..4], &61i32.to_le_bytes()); // version
        assert_eq!(&bytes[4..8], &(1i32 << 29).to_le_bytes()); // flags1: has_flags2 alone
        // flags2: are_media_timestamp_entities_found (28, always true) + has_flags3 (29)
        assert_eq!(&bytes[8..12], &((1i32 << 28) | (1i32 << 29)).to_le_bytes());
        // flags3: has_reactions_are_possible (23, always true) +
        // has_new_video_start_timestamp (25, 0 >= 0) + has_flags4 (29)
        assert_eq!(
            &bytes[12..16],
            &((1i32 << 23) | (1i32 << 25) | (1i32 << 29)).to_le_bytes()
        );
        assert_eq!(&bytes[16..20], &0i32.to_le_bytes()); // flags4: all clear

        // message_id + date ride their verified stores right after the flags
        let mut expected = Vec::new();
        m.message_id.store(&mut expected);
        crate::store_parse::store_i32(&mut expected, 100);
        assert_eq!(&bytes[20..20 + expected.len()], &expected[..]);
    }

    #[test]
    fn reply_markup_store_parse_roundtrip() {
        let markup = ReplyMarkup {
            r#type: ReplyMarkupType::ShowKeyboard,
            is_personal: true,
            force_reply: true,
            is_persistent: true,
            need_resize_keyboard: true,
            is_one_time_keyboard: true,
            keyboard: vec![vec![
                KeyboardButton {
                    r#type: KeyboardButtonType::RequestDialog,
                    style: KeyboardButtonStyle {
                        r#type: crate::messages_manager::KeyboardButtonStyleType::Primary,
                        icon_custom_emoji_id: crate::emoji_status::CustomEmojiId::new(7),
                    },
                    text: "pick".to_string(),
                    url: String::new(),
                    requested_dialog_type: Some(RequestedDialogType {
                        r#type: RequestedDialogTypeType::Group,
                        button_id: 3,
                        max_quantity: 1,
                        restrict_has_username: true,
                        has_username: true,
                        is_created: true,
                        restrict_user_administrator_rights: true,
                        ..RequestedDialogType::default()
                    }),
                },
                KeyboardButton {
                    r#type: KeyboardButtonType::Text,
                    text: "plain".to_string(),
                    ..KeyboardButton::default()
                },
            ]],
            placeholder: "type...".to_string(),
            inline_keyboard: Vec::new(),
        };
        let mut bytes = Vec::new();
        markup.store(&mut bytes);
        let mut parser = Parser::new(&bytes);
        assert_eq!(ReplyMarkup::parse_from(&mut parser).unwrap(), markup);
    }

    #[test]
    fn reply_markup_from_telegram_inline_keyboard() {
        let wire = telegram_api::ReplyMarkup::replyInlineMarkup(telegram_api::replyInlineMarkup {
            flags: 0,
            force_reply: false,
            rows: vec![telegram_api::KeyboardInlineButtonRow {
                buttons: vec![telegram_api::KeyboardInlineButton {
                    flags: 0,
                    style: None,
                    text: "open".to_string(),
                    r#type: Some(telegram_api::InlineButtonType::inlineButtonTypeUrl(
                        telegram_api::inlineButtonTypeUrl {
                            url: "https://telegram.org".to_string(),
                        },
                    )),
                }],
            }],
        });
        let markup = ReplyMarkup::from_telegram(wire, false, true, false).expect("inline keyboard");
        assert_eq!(markup.r#type, ReplyMarkupType::InlineKeyboard);
        assert_eq!(markup.inline_keyboard.len(), 1);
        let button = &markup.inline_keyboard[0][0];
        assert_eq!(button.r#type, InlineKeyboardButtonType::Url);
        assert_eq!(button.text, "open");
        assert_eq!(button.data, "https://telegram.org/"); // check_link adds the trailing slash
    }

    #[test]
    fn calls_db_state_roundtrip() {
        let state = CallsDbState {
            first_calls_database_message_id_by_index: [
                MessageId::from_server_message_id(ServerMessageId::new(100)),
                MessageId::from_server_message_id(ServerMessageId::new(200)),
            ],
            message_count_by_index: [5, -1],
        };
        let mut bytes = Vec::new();
        state.store(&mut bytes);
        let mut parser = Parser::new(&bytes);
        assert_eq!(CallsDbState::parse_from(&mut parser).unwrap(), state);
        parser.fetch_end().unwrap();

        // oversized tables are rejected
        let mut bad = Vec::new();
        store_i32(&mut bad, 3);
        let mut parser = Parser::new(&bad);
        assert!(CallsDbState::parse_from(&mut parser).is_err());
    }

    #[test]
    fn delete_message_log_event_roundtrip() {
        let event = DeleteMessageLogEvent {
            id: 0,
            message_full_id: MessageFullId::new(
                DialogId::from_user_id(UserId::new(9)),
                MessageId::from_server_message_id(ServerMessageId::new(42)),
            ),
            file_ids: vec![FileId::new(7, 0), FileId::new(8, 0)],
        };
        let mut bytes = Vec::new();
        event.store(&mut bytes);
        let mut parser = Parser::new(&bytes);
        assert_eq!(
            DeleteMessageLogEvent::parse_from(&mut parser).unwrap(),
            event
        );
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn index_mask_decision_table() {
        let mut m = Message::default();
        m.message_id = MessageId::from_server_message_id(ServerMessageId::new(1));
        m.content = Some(crate::message_content::MessageContent::Text(
            crate::message_content::MessageText::default(),
        ));

        let manager = MessagesManager::default();

        // yet-unsent messages are never indexed
        m.message_id = MessageId::min(); // a yet-unsent id
        assert_eq!(
            get_message_index_mask(
                &manager,
                &NoDialogReactionsContext,
                &crate::message_content::SharedTestContext::default(),
                DialogId::from_user_id(UserId::new(1)),
                &m
            ),
            0
        );
        // failed-to-send messages carry only the FailedToSend bit
        m.message_id = MessageId::from_server_message_id(ServerMessageId::new(2));
        m.is_failed_to_send = true;
        assert_eq!(
            get_message_index_mask(
                &manager,
                &NoDialogReactionsContext,
                &crate::message_content::SharedTestContext::default(),
                DialogId::from_user_id(UserId::new(1)),
                &m
            ),
            message_search_filter_index_mask(MessageSearchFilter::FailedToSend)
        );
        // pinned + secret content collapses to the pinned bit alone
        m.is_failed_to_send = false;
        m.is_pinned = true;
        m.is_content_secret = true;
        assert_eq!(
            get_message_index_mask(
                &manager,
                &NoDialogReactionsContext,
                &crate::message_content::SharedTestContext::default(),
                DialogId::from_user_id(UserId::new(1)),
                &m
            ),
            message_search_filter_index_mask(MessageSearchFilter::Pinned)
        );
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn parse_message_validates_the_expected_id() {
        let time = crate::user_manager::TimeCtx {
            now: 0.0,
            server_time: 0.0,
        };
        let mut m = Message::default();
        m.message_id = MessageId::from_server_message_id(ServerMessageId::new(10));
        m.date = 5;
        m.content = Some(crate::message_content::MessageContent::Text(
            crate::message_content::MessageText::default(),
        ));
        let mut bytes = Vec::new();
        store_message_log_event(&m, &MessageDbStorageBridge, &time, &mut bytes);

        // exact match
        let parsed = parse_message(&time, m.message_id, &bytes, false).unwrap();
        assert_eq!(parsed.message_id, m.message_id);
        // mismatched id is dropped
        assert!(
            parse_message(
                &time,
                MessageId::from_server_message_id(ServerMessageId::new(11)),
                &bytes,
                false
            )
            .is_none()
        );
        // a scheduled expectation over an ordinary id is dropped
        assert!(parse_message(&time, MessageId::default(), &bytes, true).is_none());
        // a corrupt blob is dropped
        assert!(parse_message(&time, m.message_id, &bytes[..bytes.len() - 1], false).is_none());
    }

    #[test]
    fn message_reactions_store_parse_roundtrip() {
        let reactions = MessageReactions {
            reactions: vec![MessageReaction {
                reaction_type: ReactionType::new("👍"),
                choose_count: 5,
                is_chosen: true,
                my_recent_chooser_dialog_id: DialogId::from_user_id(UserId::new(1)),
                recent_chooser_dialog_ids: vec![DialogId::from_user_id(UserId::new(1))],
                recent_chooser_min_channels: Vec::new(),
            }],
            unread_reactions: vec![UnreadMessageReaction {
                reaction_type: ReactionType::new("🔥"),
                sender_dialog_id: DialogId::from_user_id(UserId::new(2)),
                is_big: true,
            }],
            chosen_reaction_order: vec![ReactionType::new("👍")],
            top_reactors: vec![MessageReactor {
                dialog_id: DialogId::from_user_id(UserId::new(3)),
                min_channel: Some(MinChannel {
                    title: "top".to_string(),
                    is_megagroup: false,
                }),
                count: 7,
                is_top: true,
                is_me: false,
                is_anonymous: false,
            }],
            pending_paid_reactions: 0,
            is_min: false,
            need_polling: true,
            can_get_added_reactions: false,
            are_tags: false,
        };
        let mut bytes = Vec::new();
        reactions.store(&mut bytes);
        let mut parser = Parser::new(&bytes);
        assert_eq!(
            MessageReactions::parse_from(&mut parser).unwrap(),
            reactions
        );
        assert!(!reactions.are_empty());
        assert!(MessageReactions::default().are_empty());

        // the my-recent-chooser validity rule: a chooser not in the recent
        // list is an error
        let bad = MessageReaction {
            reaction_type: ReactionType::new("👍"),
            choose_count: 2,
            is_chosen: false,
            my_recent_chooser_dialog_id: DialogId::from_user_id(UserId::new(9)),
            recent_chooser_dialog_ids: vec![DialogId::from_user_id(UserId::new(1))],
            recent_chooser_min_channels: Vec::new(),
        };
        let mut bytes = Vec::new();
        bad.store(&mut bytes);
        let mut parser = Parser::new(&bytes);
        assert!(MessageReaction::parse_from(&mut parser).is_err());
    }
}
