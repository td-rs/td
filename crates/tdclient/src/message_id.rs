//! Minimal port of the C++ `td/telegram/DialogId.h`, `MessageId.h`,
//! `MessageFullId.h` and the `ChannelId` newtype — the subset the
//! UserManager contact-policy step (M.4.15) needs for the message-users
//! and noforwards-request registries. The three C++ headers are ~400 lines
//! together; one module holds the landed subset with the deferred parts
//! documented below.
//!
//! Deferred with their owners:
//! - the `telegram_api::Peer`/`DialogPeer`/`Message` conversions
//!   (`DialogId::get_message_dialog_id`, `MessageId::get_message_id`,
//!   `MessageFullId::get_message_full_id`, `get_max_message_id`,
//!   `is_message_id_order_*`) land with the MessagesManager step, which
//!   owns the message intake;
//! - `MessageId`'s full layout (`MessageType`, the server/scheduled/sponsored
//!   bit packing, `get_server_message_id`/`get_scheduled_message_date`/
//!   `get_next_message_id` and the comparison operators) lands with the
//!   message-sending step; the registry only needs `is_scheduled` and the
//!   raw value;
//! - the `ChatId`/`SecretChatId` dialog conversions and `get_dialog_ids`
//!   land with the ChatManager/SecretChats steps;
//! - `DialogId::get_simple_input_peer` is ChatManager's (the
//!   `inputPeerUserFromMessage` construction in the UserManager read
//!   surface waits for it).

use crate::store_parse::{self, Parser};
use crate::user_manager::UserId;

/// `td::DialogType` (DialogId.h:23).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DialogType {
    None,
    User,
    Chat,
    Channel,
    SecretChat,
}

/// `td::ChannelId` (ChannelId.h) — the subset the channel-gate check
/// needs; the full validity ranges (regular vs monoforum) land with the
/// ChatManager step.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChannelId(i64);

impl ChannelId {
    /// `ChannelId::MAX_CHANNEL_ID` (ChannelId.h:24).
    pub const MAX_CHANNEL_ID: i64 = 1000000000000i64 - (1i64 << 31);
    /// `ChannelId::MIN_MONOFORUM_CHANNEL_ID` (ChannelId.h:25).
    pub const MIN_MONOFORUM_CHANNEL_ID: i64 = 1000000000000i64 + (1i64 << 31) + 1;

    pub fn new(id: i64) -> ChannelId {
        ChannelId(id)
    }

    /// `ChannelId::is_valid` (ChannelId.h:34-36).
    pub fn is_valid(self) -> bool {
        (0 < self.0 && self.0 < Self::MAX_CHANNEL_ID)
            || (Self::MIN_MONOFORUM_CHANNEL_ID <= self.0 && self.0 < Self::MAX_MONOFORUM_CHANNEL_ID)
    }

    pub fn get(self) -> i64 {
        self.0
    }
}

impl ChannelId {
    /// `ChannelId::MAX_MONOFORUM_CHANNEL_ID` (ChannelId.h:26).
    const MAX_MONOFORUM_CHANNEL_ID: i64 = 3000000000000i64;
}

/// `td::DialogId` (DialogId.h) — the id ranges and the type derivation;
/// the peer/message conversions are the MessagesManager step (module
/// docs).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DialogId(i64);

impl DialogId {
    /// `DialogId::ZERO_SECRET_CHAT_ID` (DialogId.h:26).
    pub const ZERO_SECRET_CHAT_ID: i64 = -2000000000000i64;
    /// `DialogId::ZERO_CHANNEL_ID` (DialogId.h:27).
    pub const ZERO_CHANNEL_ID: i64 = -1000000000000i64;
    /// `ChatId::MAX_CHAT_ID` (ChatId.h:23).
    const MAX_CHAT_ID: i64 = 999999999999;

    pub fn new(id: i64) -> DialogId {
        DialogId(id)
    }

    pub fn get(self) -> i64 {
        self.0
    }

    /// `DialogId::is_valid` (DialogId.cpp:19-21).
    pub fn is_valid(self) -> bool {
        self.get_type() != DialogType::None
    }

    /// `DialogId::get_type` (DialogId.cpp:23-45) — the continuous-range
    /// table, in the C++ branch order (the monoforum channel band is
    /// checked after the secret-chat band).
    pub fn get_type(self) -> DialogType {
        if self.0 < 0 {
            if -Self::MAX_CHAT_ID <= self.0 {
                return DialogType::Chat;
            }
            if Self::ZERO_CHANNEL_ID - ChannelId::MAX_CHANNEL_ID <= self.0
                && self.0 != Self::ZERO_CHANNEL_ID
            {
                return DialogType::Channel;
            }
            if Self::ZERO_SECRET_CHAT_ID + i32::MIN as i64 <= self.0
                && self.0 != Self::ZERO_SECRET_CHAT_ID
            {
                return DialogType::SecretChat;
            }
            if Self::ZERO_CHANNEL_ID - ChannelId::MIN_MONOFORUM_CHANNEL_ID <= self.0 {
                return DialogType::Channel;
            }
        } else if 0 < self.0 && self.0 <= UserId::MAX_USER_ID {
            return DialogType::User;
        }
        DialogType::None
    }

    /// `DialogId::get_channel_id` (DialogId.cpp:53-56); the C++ `CHECK`
    /// is the assert.
    pub fn get_channel_id(self) -> ChannelId {
        assert!(self.get_type() == DialogType::Channel);
        ChannelId::new(Self::ZERO_CHANNEL_ID - self.0)
    }

    /// `DialogId::store` (DialogId.h:86-88).
    pub fn store(self, out: &mut Vec<u8>) {
        store_parse::store_i64(out, self.0);
    }

    /// `DialogId::parse` (DialogId.h:90-93).
    pub fn parse_from(parser: &mut Parser) -> Result<DialogId, &'static str> {
        Ok(DialogId::new(parser.i64_()?))
    }
}

/// `td::MessageId` (MessageId.h) — the id newtype with the subset the
/// noforwards registry needs (`is_scheduled`, the raw value); the full
/// layout arrives with the message-sending step (module docs).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MessageId(i64);

impl MessageId {
    /// `MessageId::SCHEDULED_MASK` (MessageId.h:31).
    const SCHEDULED_MASK: i64 = 4;

    pub fn new(id: i64) -> MessageId {
        MessageId(id)
    }

    pub fn get(self) -> i64 {
        self.0
    }

    /// `MessageId::is_scheduled` (MessageId.h:107-109).
    pub fn is_scheduled(self) -> bool {
        (self.0 & Self::SCHEDULED_MASK) != 0
    }

    /// `MessageId::store` (MessageId.h:193-195).
    pub fn store(self, out: &mut Vec<u8>) {
        store_parse::store_i64(out, self.0);
    }

    /// `MessageId::parse` (MessageId.h:197-199).
    pub fn parse_from(parser: &mut Parser) -> Result<MessageId, &'static str> {
        Ok(MessageId::new(parser.i64_()?))
    }
}

/// `td::MessageFullId` (MessageFullId.h) — the dialog + message pair.
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

    /// `MessageFullId::get_dialog_id` (MessageFullId.h:35-37).
    pub fn get_dialog_id(self) -> DialogId {
        self.dialog_id
    }

    /// `MessageFullId::get_message_id` (MessageFullId.h:39-41).
    pub fn get_message_id(self) -> MessageId {
        self.message_id
    }

    /// `MessageFullId::store` (MessageFullId.h:52-55) — dialog then
    /// message, two store_longs.
    pub fn store(self, out: &mut Vec<u8>) {
        self.dialog_id.store(out);
        self.message_id.store(out);
    }

    /// `MessageFullId::parse` (MessageFullId.h:57-60).
    pub fn parse_from(parser: &mut Parser) -> Result<MessageFullId, &'static str> {
        Ok(MessageFullId::new(
            DialogId::parse_from(parser)?,
            MessageId::parse_from(parser)?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dialog_id_derives_the_cpp_type_ranges() {
        // the C++ static_asserts: the ranges are continuous and abut
        assert_eq!(DialogId::ZERO_CHANNEL_ID + 1, -DialogId::MAX_CHAT_ID);
        assert_eq!(
            DialogId::ZERO_SECRET_CHAT_ID + i32::MAX as i64 + 1,
            DialogId::ZERO_CHANNEL_ID - ChannelId::MAX_CHANNEL_ID
        );
        assert_eq!(
            DialogId::ZERO_CHANNEL_ID - ChannelId::MIN_MONOFORUM_CHANNEL_ID + 1,
            DialogId::ZERO_SECRET_CHAT_ID + i32::MIN as i64
        );

        assert_eq!(DialogId::new(0).get_type(), DialogType::None);
        assert_eq!(DialogId::new(777000).get_type(), DialogType::User);
        assert_eq!(DialogId::new(-1).get_type(), DialogType::Chat);
        // regular channel: -1000000000000 - (1 << 20) inside the band
        let channel_id = 1000000000000i64 - (1i64 << 20);
        let channel_dialog = DialogId::new(DialogId::ZERO_CHANNEL_ID - channel_id);
        assert_eq!(channel_dialog.get_type(), DialogType::Channel);
        assert_eq!(channel_dialog.get_channel_id(), ChannelId::new(channel_id));
        // monoforum channel: the band below the secret-chat range
        let monoforum_id = ChannelId::MIN_MONOFORUM_CHANNEL_ID + 5;
        let monoforum_dialog = DialogId::new(DialogId::ZERO_CHANNEL_ID - monoforum_id);
        assert_eq!(monoforum_dialog.get_type(), DialogType::Channel);
        assert_eq!(
            monoforum_dialog.get_channel_id(),
            ChannelId::new(monoforum_id)
        );
        // secret chat: -2000000000000 - 100
        let secret_dialog = DialogId::new(DialogId::ZERO_SECRET_CHAT_ID - 100);
        assert_eq!(secret_dialog.get_type(), DialogType::SecretChat);
        assert!(!DialogId::new(DialogId::ZERO_CHANNEL_ID).is_valid());
        assert!(!DialogId::new(DialogId::ZERO_SECRET_CHAT_ID).is_valid());
        assert!(channel_dialog.is_valid());
    }

    #[test]
    fn message_full_id_store_parse_is_two_longs() {
        let message_full_id = MessageFullId::new(
            DialogId::new(DialogId::ZERO_CHANNEL_ID - 7),
            MessageId::new(1 << 20),
        );
        assert!(!message_full_id.get_message_id().is_scheduled());
        assert!(MessageId::new((1 << 20) | 4).is_scheduled());

        let mut out = Vec::new();
        message_full_id.store(&mut out);
        // the C++ `store`: dialog_id.store_long + message_id.store_long
        assert_eq!(out.len(), 16);
        let mut parser = Parser::new(&out);
        let parsed = MessageFullId::parse_from(&mut parser).expect("parses");
        parser.fetch_end().expect("fully consumed");
        assert_eq!(parsed, message_full_id);
    }
}
