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

use crate::chat_manager::{ChannelId, ChatId};
use crate::message_entity::FormattedText;
use crate::store_parse::{Parser, store_flags, store_i32, store_i64, store_string};
use crate::td_api;
use crate::user_manager::{SecretChatId, UserId};
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
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
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
                    DialogId::from_peer(&m.peer_id)
                }
            }
            telegram_api::Message::message(m) => DialogId::from_peer(&m.peer_id),
            telegram_api::Message::messageService(m) => DialogId::from_peer(&m.peer_id),
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
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
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
                        peer: Box::new(input_peer),
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

/// `td::MessageContentType` (MessageContentType.h:15-113) — the discriminant
/// order is the C++ enum order; `MessageUnsupported::CURRENT_VERSION` bumps
/// with every addition. The classification-function family
/// (`is_service_message_content` etc., MessageContentType.cpp) lands with the
/// M.9 content model; only the predicate `MessageSelfDestructType` needs is
/// ported here.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[repr(i32)]
pub enum MessageContentType {
    None = -1,
    #[default]
    Text = 0,
    Animation,
    Audio,
    Document,
    Photo,
    Sticker,
    Video,
    VoiceNote,
    Contact,
    Location,
    Venue,
    ChatCreate,
    ChatChangeTitle,
    ChatChangePhoto,
    ChatDeletePhoto,
    ChatDeleteHistory,
    ChatAddUsers,
    ChatJoinedByLink,
    ChatDeleteUser,
    ChatMigrateTo,
    ChannelCreate,
    ChannelMigrateFrom,
    PinMessage,
    Game,
    GameScore,
    ScreenshotTaken,
    ChatSetTtl,
    Unsupported,
    Call,
    Invoice,
    PaymentSuccessful,
    VideoNote,
    ContactRegistered,
    ExpiredPhoto,
    ExpiredVideo,
    LiveLocation,
    CustomServiceAction,
    WebsiteConnected,
    PassportDataSent,
    PassportDataReceived,
    Poll,
    Dice,
    ProximityAlertTriggered,
    GroupCall,
    InviteToGroupCall,
    ChatSetTheme,
    WebViewDataSent,
    WebViewDataReceived,
    GiftPremium,
    TopicCreate,
    TopicEdit,
    SuggestProfilePhoto,
    WriteAccessAllowed,
    RequestedDialog,
    WebViewWriteAccessAllowed,
    SetBackground,
    Story,
    WriteAccessAllowedByRequest,
    GiftCode,
    Giveaway,
    GiveawayLaunch,
    GiveawayResults,
    GiveawayWinners,
    ExpiredVideoNote,
    ExpiredVoiceNote,
    BoostApply,
    DialogShared,
    PaidMedia,
    PaymentRefunded,
    GiftStars,
    PrizeStars,
    StarGift,
    StarGiftUnique,
    PaidMessagesRefunded,
    PaidMessagesPrice,
    ConferenceCall,
    ToDoList,
    TodoCompletions,
    TodoAppendTasks,
    GiftTon,
    SuggestedPostSuccess,
    SuggestedPostRefund,
    SuggestedPostApproval,
    SuggestBirthday,
    StarGiftPurchaseOffer,
    StarGiftPurchaseOfferDeclined,
    NewCreatorPending,
    ChangeCreator,
    NoForwardsToggle,
    NoForwardsRequest,
    ManagedBotCreated,
    PollAppendAnswer,
    PollDeleteAnswer,
    RichText,
    ChangeCommunity,
    ChatJoinedViaCommunity,
}

/// `can_be_secret_message_content` (MessageContentType.cpp) — the C++
/// `default: UNREACHABLE()` covers only `None`.
pub fn can_be_secret_message_content(content_type: MessageContentType) -> bool {
    use MessageContentType::*;
    matches!(
        content_type,
        Animation | Audio | Photo | Video | VideoNote | VoiceNote
    )
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
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
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

/// `td::MessageReactions` — STAND-IN: the full struct is owned by the Wave-2
/// ReactionManager slice; until then the `Message`/`Dialog` fields only need
/// an opaque owned value (MessagesManager.h:127 forward-declares it).
#[derive(Clone, Debug, Default)]
pub struct MessageReactions {
    // ported with the Wave-2 reactions slice
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
                    peer_id: peer.clone(),
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
            peer_id: peer.clone(),
        });
        assert_eq!(DialogId::get_message_dialog_id(&empty_with_peer), user);

        let service = telegram_api::Message::messageService(telegram_api::messageService {
            id: 1,
            date: 1,
            peer_id: peer,
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
            peer_id: telegram_api::Peer::peerUser(telegram_api::peerUser { user_id: 7 }),
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
}
