//! Port of the dialog/message id and search-filter tail shared by the
//! DialogManager/MessageDb slice (M.6): `td/telegram/DialogId.{h,cpp}`,
//! `MessageFullId.h`, `ServerMessageId.h`, `ScheduledServerMessageId.h`,
//! `NotificationId.h`, `NotificationGroupId.h`, `FolderId.h` and
//! `MessageSearchFilter.{h,cpp}`.
//!
//! `MessageId` itself lives in [`crate::chat_manager`] (it landed there as
//! the td-side stand-in; its full bit layout is on that type). The
//! `td_api`/`telegram_api` filter conversions (`get_input_messages_filter`,
//! `get_message_search_filter`, MessageSearchFilter.cpp:31-102) land with
//! the td_api request routing (M.6.20) — nothing in this slice consumes
//! them yet.

use crate::chat_manager::{ChannelId, ChatId, MessageId};
use crate::store_parse::{self, Parser};
use crate::user_manager::{SecretChatId, UserId};
use tdmtproto::telegram_api;

/// `td::DialogType` (DialogId.h:23).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DialogType {
    #[default]
    None,
    User,
    Chat,
    Channel,
    SecretChat,
}

/// `td::DialogId` (DialogId.h) — the id-range type table plus the
/// `telegram_api::Peer`/`DialogPeer`/`Message` conversions; the
/// `get_dialog_ids` batch helpers (DialogId.cpp:213-250) wait for their
/// first consumer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DialogId(i64);

impl DialogId {
    /// `DialogId::ZERO_SECRET_CHAT_ID` (DialogId.h:26).
    pub const ZERO_SECRET_CHAT_ID: i64 = -2_000_000_000_000;
    /// `DialogId::ZERO_CHANNEL_ID` (DialogId.h:27).
    pub const ZERO_CHANNEL_ID: i64 = -1_000_000_000_000;

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

    /// `DialogId::get_type` (DialogId.cpp:23-45). The C++ static_asserts
    /// pin the bands as continuous; the monoforum channel band (everything
    /// above `ZERO_CHANNEL_ID - MAX_MONOFORUM_CHANNEL_ID`) is checked last,
    /// in the C++ branch order.
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
            if Self::ZERO_SECRET_CHAT_ID + i32::MIN as i64 <= self.0
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

    /// `DialogId::get_user_id` (DialogId.cpp:46-49); the C++ `CHECK` is the
    /// assert.
    pub fn get_user_id(self) -> UserId {
        assert!(self.get_type() == DialogType::User);
        UserId::new(self.0)
    }

    /// `DialogId::get_chat_id` (DialogId.cpp:51-54).
    pub fn get_chat_id(self) -> ChatId {
        assert!(self.get_type() == DialogType::Chat);
        ChatId::new(-self.0)
    }

    /// `DialogId::get_channel_id` (DialogId.cpp:56-59).
    pub fn get_channel_id(self) -> ChannelId {
        assert!(self.get_type() == DialogType::Channel);
        ChannelId::new(Self::ZERO_CHANNEL_ID - self.0)
    }

    /// `DialogId::get_secret_chat_id` (DialogId.cpp:61-64).
    pub fn get_secret_chat_id(self) -> SecretChatId {
        assert!(self.get_type() == DialogType::SecretChat);
        SecretChatId::new((self.0 - Self::ZERO_SECRET_CHAT_ID) as i32)
    }

    /// `DialogId::from(UserId)` (DialogId.cpp:66-72) — invalid ids become 0.
    pub fn from_user_id(user_id: UserId) -> DialogId {
        DialogId::new(if user_id.is_valid() { user_id.get() } else { 0 })
    }

    /// `DialogId::from(ChatId)` (DialogId.cpp:74-80).
    pub fn from_chat_id(chat_id: ChatId) -> DialogId {
        DialogId::new(if chat_id.is_valid() {
            -chat_id.get()
        } else {
            0
        })
    }

    /// `DialogId::from(ChannelId)` (DialogId.cpp:82-88).
    pub fn from_channel_id(channel_id: ChannelId) -> DialogId {
        DialogId::new(if channel_id.is_valid() {
            Self::ZERO_CHANNEL_ID - channel_id.get()
        } else {
            0
        })
    }

    /// `DialogId::from(SecretChatId)` (DialogId.cpp:90-96).
    pub fn from_secret_chat_id(secret_chat_id: SecretChatId) -> DialogId {
        DialogId::new(if secret_chat_id.is_valid() {
            Self::ZERO_SECRET_CHAT_ID + secret_chat_id.get() as i64
        } else {
            0
        })
    }

    /// `DialogId::get_peer_id` (DialogId.cpp:121-155) — the C++
    /// `UNREACHABLE()` default is unreachable because the `Peer` enum is
    /// closed over the same three constructors.
    pub fn get_peer_id(peer: &telegram_api::Peer) -> i64 {
        match peer {
            telegram_api::Peer::peerUser(peer) => {
                let user_id = UserId::new(peer.user_id);
                if !user_id.is_valid() {
                    tracing::error!("Receive invalid {user_id:?}");
                    return 0;
                }
                user_id.get()
            }
            telegram_api::Peer::peerChat(peer) => {
                let chat_id = ChatId::new(peer.chat_id);
                if !chat_id.is_valid() {
                    tracing::error!("Receive invalid {chat_id:?}");
                    return 0;
                }
                -chat_id.get()
            }
            telegram_api::Peer::peerChannel(peer) => {
                let channel_id = ChannelId::new(peer.channel_id);
                if !channel_id.is_valid() {
                    tracing::error!("Receive invalid {channel_id:?}");
                    return 0;
                }
                Self::ZERO_CHANNEL_ID - channel_id.get()
            }
        }
    }

    /// `DialogId::from(peer)` (DialogId.cpp:158).
    pub fn from_peer(peer: &telegram_api::Peer) -> DialogId {
        DialogId::new(Self::get_peer_id(peer))
    }

    /// `DialogId::from(dialog_peer)` (DialogId.cpp:98-116) — the folder and
    /// community constructors are unsupported and reset to 0 with a logged
    /// error, like the C++.
    pub fn from_dialog_peer(dialog_peer: &telegram_api::DialogPeer) -> DialogId {
        match dialog_peer {
            telegram_api::DialogPeer::dialogPeer(peer) => DialogId::from_peer(&peer.peer),
            telegram_api::DialogPeer::dialogPeerFolder(_) => {
                tracing::error!("Receive unsupported dialogPeerFolder");
                DialogId::new(0)
            }
            telegram_api::DialogPeer::dialogPeerCommunity(_) => {
                tracing::error!("Receive unsupported dialogPeerCommunity");
                DialogId::new(0)
            }
        }
    }

    /// `DialogId::get_message_dialog_id` (DialogId.cpp:157-181). The C++
    /// `peer_id_ == nullptr` check is the `peer_id` flag bit (flags.0) on
    /// `messageEmpty` — the generated model carries the field without the
    /// Option.
    pub fn get_message_dialog_id(message: &telegram_api::Message) -> DialogId {
        match message {
            telegram_api::Message::messageEmpty(message) => {
                if (message.flags & 1) != 0 {
                    DialogId::from_peer(&message.peer_id)
                } else {
                    DialogId::new(0)
                }
            }
            telegram_api::Message::message(message) => DialogId::from_peer(&message.peer_id),
            telegram_api::Message::messageService(message) => DialogId::from_peer(&message.peer_id),
        }
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

/// `td::ServerMessageId` (ServerMessageId.h).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ServerMessageId(i32);

impl ServerMessageId {
    pub fn new(id: i32) -> ServerMessageId {
        ServerMessageId(id)
    }

    /// `ServerMessageId::is_valid`.
    pub fn is_valid(self) -> bool {
        self.0 > 0
    }

    pub fn get(self) -> i32 {
        self.0
    }
}

/// `td::ScheduledServerMessageId` (ScheduledServerMessageId.h).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ScheduledServerMessageId(i32);

impl ScheduledServerMessageId {
    pub fn new(id: i32) -> ScheduledServerMessageId {
        ScheduledServerMessageId(id)
    }

    /// `ScheduledServerMessageId::is_valid`.
    pub fn is_valid(self) -> bool {
        self.0 > 0 && self.0 < (1 << 18)
    }

    pub fn get(self) -> i32 {
        self.0
    }
}

/// `td::MessageFullId` (MessageFullId.h) — the dialog + message pair; the
/// `telegram_api::Message` constructor (`get_message_full_id`) lands with
/// the message intake (MessagesManager step), it only composes the two
/// pieces above.
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

    /// `MessageFullId::get_dialog_id` (MessageFullId.h).
    pub fn get_dialog_id(self) -> DialogId {
        self.dialog_id
    }

    /// `MessageFullId::get_message_id` (MessageFullId.h).
    pub fn get_message_id(self) -> MessageId {
        self.message_id
    }

    /// `MessageFullId::store` (MessageFullId.h) — dialog then message, two
    /// store_longs.
    pub fn store(self, out: &mut Vec<u8>) {
        self.dialog_id.store(out);
        self.message_id.store(out);
    }

    /// `MessageFullId::parse` (MessageFullId.h).
    pub fn parse_from(parser: &mut Parser) -> Result<MessageFullId, &'static str> {
        Ok(MessageFullId::new(
            DialogId::parse_from(parser)?,
            MessageId::parse_from(parser)?,
        ))
    }
}

/// `td::NotificationId` (NotificationId.h).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NotificationId(i32);

impl NotificationId {
    /// `NotificationId::max`.
    pub const MAX: NotificationId = NotificationId(i32::MAX);

    pub fn new(id: i32) -> NotificationId {
        NotificationId(id)
    }

    /// `NotificationId::is_valid`.
    pub fn is_valid(self) -> bool {
        self.0 > 0
    }

    pub fn get(self) -> i32 {
        self.0
    }

    /// `NotificationId::store` (NotificationId.h).
    pub fn store(self, out: &mut Vec<u8>) {
        store_parse::store_i32(out, self.0);
    }

    /// `NotificationId::parse` (NotificationId.h).
    pub fn parse_from(parser: &mut Parser) -> Result<NotificationId, &'static str> {
        Ok(NotificationId::new(parser.i32_()?))
    }
}

/// `td::NotificationGroupId` (NotificationGroupId.h).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NotificationGroupId(i32);

impl NotificationGroupId {
    pub fn new(id: i32) -> NotificationGroupId {
        NotificationGroupId(id)
    }

    /// `NotificationGroupId::is_valid`.
    pub fn is_valid(self) -> bool {
        self.0 > 0
    }

    pub fn get(self) -> i32 {
        self.0
    }

    /// `NotificationGroupId::store` (NotificationGroupId.h).
    pub fn store(self, out: &mut Vec<u8>) {
        store_parse::store_i32(out, self.0);
    }

    /// `NotificationGroupId::parse` (NotificationGroupId.h).
    pub fn parse_from(parser: &mut Parser) -> Result<NotificationGroupId, &'static str> {
        Ok(NotificationGroupId::new(parser.i32_()?))
    }
}

/// `td::FolderId` (FolderId.h) — the constructor clamps everything but the
/// archive folder to the main one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FolderId(i32);

impl FolderId {
    pub const MAIN: FolderId = FolderId(0);
    pub const ARCHIVE: FolderId = FolderId(1);

    pub fn new(id: i32) -> FolderId {
        FolderId(if id == 1 { 1 } else { 0 })
    }

    pub fn get(self) -> i32 {
        self.0
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dialog_id_derives_the_cpp_type_ranges() {
        // the C++ static_asserts (DialogId.cpp:15-20): the bands are
        // continuous and abut
        assert_eq!(DialogId::ZERO_CHANNEL_ID + 1, -ChatId::MAX_CHAT_ID);
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
        assert_eq!(DialogId::new(1 << 40).get_type(), DialogType::None);
        assert_eq!(DialogId::new(-1).get_type(), DialogType::Chat);
        assert_eq!(
            DialogId::new(-ChatId::MAX_CHAT_ID).get_type(),
            DialogType::Chat
        );
        // C++ quirk: the id exactly at ZERO_CHANNEL_ID falls through the
        // channel check (the `!= ZERO_CHANNEL_ID` guard) into the
        // unclamped-above secret-chat band
        assert_eq!(
            DialogId::new(-ChatId::MAX_CHAT_ID - 1).get_type(),
            DialogType::SecretChat
        );
        // the None gap below the monoforum channel band (which spans down
        // to ZERO_CHANNEL_ID - MAX_MONOFORUM_CHANNEL_ID)
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
        assert_eq!(
            DialogId::new(DialogId::ZERO_CHANNEL_ID - ChannelId::MIN_MONOFORUM_CHANNEL_ID)
                .get_type(),
            DialogType::Channel
        );

        // regular channel: inside the band below ZERO_CHANNEL_ID
        let channel_id = 1i64 << 20;
        let channel_dialog = DialogId::new(DialogId::ZERO_CHANNEL_ID - channel_id);
        assert_eq!(channel_dialog.get_type(), DialogType::Channel);
        assert_eq!(channel_dialog.get_channel_id(), ChannelId::new(channel_id));
        // the ZERO_CHANNEL_ID exclusion again: it classifies as SecretChat
        // and is_valid() (get_type() != None) is therefore TRUE for it
        assert_eq!(
            DialogId::new(DialogId::ZERO_CHANNEL_ID).get_type(),
            DialogType::SecretChat
        );
        assert!(DialogId::new(DialogId::ZERO_CHANNEL_ID).is_valid());

        // monoforum channel: the band checked after the secret-chat band
        let monoforum_id = ChannelId::MIN_MONOFORUM_CHANNEL_ID + 5;
        let monoforum_dialog = DialogId::new(DialogId::ZERO_CHANNEL_ID - monoforum_id);
        assert_eq!(monoforum_dialog.get_type(), DialogType::Channel);
        assert_eq!(
            monoforum_dialog.get_channel_id(),
            ChannelId::new(monoforum_id)
        );

        // secret chat: the band spans [ZERO + i32::MIN, ZERO + i32::MAX];
        // the round trip is via `id - ZERO`, so a *negative* offset yields a
        // negative (invalid) SecretChatId — the C++ band check does not
        // validate the extracted SecretChatId
        let secret_dialog = DialogId::new(DialogId::ZERO_SECRET_CHAT_ID + 100);
        assert_eq!(secret_dialog.get_type(), DialogType::SecretChat);
        assert_eq!(secret_dialog.get_secret_chat_id(), SecretChatId::new(100));
        let negative_secret_dialog = DialogId::new(DialogId::ZERO_SECRET_CHAT_ID - 100);
        assert_eq!(negative_secret_dialog.get_type(), DialogType::SecretChat);
        assert_eq!(
            negative_secret_dialog.get_secret_chat_id(),
            SecretChatId::new(-100)
        );
        // C++ quirk #2: ZERO_SECRET_CHAT_ID is skipped by the secret-chat
        // check (the `!= ZERO_SECRET_CHAT_ID` guard) and then falls into the
        // monoforum channel band ([ZERO - MAX_MONOFORUM, ZERO -
        // MIN_MONOFORUM]), so it types as Channel and is_valid()
        assert_eq!(
            DialogId::new(DialogId::ZERO_SECRET_CHAT_ID).get_type(),
            DialogType::Channel
        );
        assert!(DialogId::new(DialogId::ZERO_SECRET_CHAT_ID).is_valid());
        assert!(channel_dialog.is_valid());
        assert!(secret_dialog.is_valid());

        // the from_* constructors zero invalid ids (DialogId.cpp:66-96)
        assert_eq!(DialogId::from_user_id(UserId::new(0)), DialogId::new(0));
        assert_eq!(DialogId::from_user_id(UserId::new(42)), DialogId::new(42));
        assert_eq!(DialogId::from_chat_id(ChatId::new(42)), DialogId::new(-42));
        assert_eq!(
            DialogId::from_channel_id(ChannelId::new(channel_id)),
            channel_dialog
        );
        assert_eq!(
            DialogId::from_secret_chat_id(SecretChatId::new(100)),
            DialogId::new(DialogId::ZERO_SECRET_CHAT_ID + 100)
        );
        assert_eq!(
            DialogId::from_secret_chat_id(SecretChatId::new(0)),
            DialogId::new(0)
        );
    }

    #[test]
    fn dialog_id_peer_conversion_matches_the_cpp_ranges() {
        let peer_user = telegram_api::Peer::peerUser(telegram_api::peerUser { user_id: 42 });
        assert_eq!(DialogId::from_peer(&peer_user), DialogId::new(42));
        let peer_chat = telegram_api::Peer::peerChat(telegram_api::peerChat { chat_id: 42 });
        assert_eq!(DialogId::from_peer(&peer_chat), DialogId::new(-42));
        let channel_id = 1i64 << 20;
        let peer_channel =
            telegram_api::Peer::peerChannel(telegram_api::peerChannel { channel_id });
        assert_eq!(
            DialogId::from_peer(&peer_channel),
            DialogId::new(DialogId::ZERO_CHANNEL_ID - channel_id)
        );
        // invalid ids log and reset to 0
        let invalid_user = telegram_api::Peer::peerUser(telegram_api::peerUser { user_id: -1 });
        assert_eq!(DialogId::from_peer(&invalid_user), DialogId::new(0));
        // above MAX_CHANNEL_ID the channel id is invalid
        let out_of_range_channel = telegram_api::Peer::peerChannel(telegram_api::peerChannel {
            channel_id: 1_000_000_000_000 - (1i64 << 20),
        });
        assert_eq!(DialogId::from_peer(&out_of_range_channel), DialogId::new(0));
    }

    #[test]
    fn message_full_id_store_parse_is_two_longs() {
        let message_full_id = MessageFullId::new(
            DialogId::new(DialogId::ZERO_CHANNEL_ID - 7),
            MessageId::new(1 << 20),
        );
        let mut out = Vec::new();
        message_full_id.store(&mut out);
        // the C++ `store`: dialog_id.store_long + message_id.store_long
        assert_eq!(out.len(), 16);
        let mut parser = Parser::new(&out);
        let parsed = MessageFullId::parse_from(&mut parser).expect("parses");
        parser.fetch_end().expect("fully consumed");
        assert_eq!(parsed, message_full_id);
    }

    #[test]
    fn notification_and_folder_ids_keep_the_cpp_shapes() {
        assert!(!NotificationId::new(0).is_valid());
        assert!(NotificationId::new(1).is_valid());
        assert_eq!(NotificationId::MAX.get(), i32::MAX);
        let mut out = Vec::new();
        NotificationId::new(-7).store(&mut out);
        let mut parser = Parser::new(&out);
        assert_eq!(
            NotificationId::parse_from(&mut parser).unwrap(),
            NotificationId::new(-7)
        );
        parser.fetch_end().expect("fully consumed");

        let mut out = Vec::new();
        NotificationGroupId::new(9).store(&mut out);
        let mut parser = Parser::new(&out);
        assert_eq!(
            NotificationGroupId::parse_from(&mut parser).unwrap(),
            NotificationGroupId::new(9)
        );
        parser.fetch_end().expect("fully consumed");

        // the FolderId constructor clamps everything but archive to main
        assert_eq!(FolderId::new(0), FolderId::MAIN);
        assert_eq!(FolderId::new(1), FolderId::ARCHIVE);
        assert_eq!(FolderId::new(2), FolderId::MAIN);
        assert_eq!(FolderId::new(-5), FolderId::MAIN);
    }

    #[test]
    fn message_search_filter_indexes_are_bijective_over_the_mask_space() {
        assert_eq!(message_search_filter_count(), 20);
        assert_eq!(
            message_search_filter_index_mask(MessageSearchFilter::Empty),
            0
        );
        // the index space matches the C++ static ordering, 0..=19
        for ordinal in 1..MessageSearchFilter::SIZE {
            let filter = match ordinal {
                0 => MessageSearchFilter::Empty,
                1 => MessageSearchFilter::Animation,
                2 => MessageSearchFilter::Audio,
                3 => MessageSearchFilter::Document,
                4 => MessageSearchFilter::Photo,
                5 => MessageSearchFilter::Video,
                6 => MessageSearchFilter::VoiceNote,
                7 => MessageSearchFilter::PhotoAndVideo,
                8 => MessageSearchFilter::Url,
                9 => MessageSearchFilter::ChatPhoto,
                10 => MessageSearchFilter::Call,
                11 => MessageSearchFilter::MissedCall,
                12 => MessageSearchFilter::VideoNote,
                13 => MessageSearchFilter::VoiceAndVideoNote,
                14 => MessageSearchFilter::Mention,
                15 => MessageSearchFilter::UnreadMention,
                16 => MessageSearchFilter::FailedToSend,
                17 => MessageSearchFilter::Pinned,
                18 => MessageSearchFilter::UnreadReaction,
                19 => MessageSearchFilter::Poll,
                20 => MessageSearchFilter::UnreadPollVote,
                _ => unreachable!(),
            };
            assert_eq!(filter.as_i32(), ordinal);
            assert_eq!(message_search_filter_index(filter), ordinal - 1);
            assert_eq!(message_search_filter_index_mask(filter), 1 << (ordinal - 1));
        }
        assert_eq!(
            call_message_search_filter_index(MessageSearchFilter::Call),
            0
        );
        assert_eq!(
            call_message_search_filter_index(MessageSearchFilter::MissedCall),
            1
        );
    }
}
