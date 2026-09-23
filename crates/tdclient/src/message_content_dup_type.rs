//! Port of `td/telegram/MessageContentDupType.h` — the six-way tag that
//! tells message-content copiers which duplication is happening.
//!
//! The C++ header belongs to the message-content plane, but it is a
//! free-standing 6-variant append-only enum with no dependencies; the
//! ReplyMarkup dup machinery ([`crate::reply_markup::dup_reply_markup`],
//! `InlineKeyboardButton::clone_button`) consumes it, so it lands here
//! first. At the M.10 integration the message-content owner adopts this
//! module verbatim (append-only; the discriminants are load-bearing
//! only as a `static_cast<int32>` mirror of the C++ enum).

/// `td::MessageContentDupType` (MessageContentDupType.h:8) — append only.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MessageContentDupType {
    /// Normal message sending.
    #[default]
    Send,
    /// Message sending via bot.
    SendViaBot,
    /// Quick reply sending.
    SendQuickReply,
    /// Server-side message forward.
    Forward,
    /// Local message copy.
    Copy,
    /// Server-side message copy.
    ServerCopy,
}

impl MessageContentDupType {
    /// `MessageContentDupType::is_send`-shaped check
    /// (`dup_type == Send || dup_type == SendViaBot ||
    /// dup_type == SendQuickReply`, ReplyMarkup.cpp:341 and
    /// InlineKeyboardButton.cpp:48).
    pub fn is_send(self) -> bool {
        matches!(
            self,
            MessageContentDupType::Send
                | MessageContentDupType::SendViaBot
                | MessageContentDupType::SendQuickReply
        )
    }

    #[allow(dead_code)] // the store-side of the mirror; the store/parse consumers arrive with M.10
    fn as_i32(self) -> i32 {
        match self {
            MessageContentDupType::Send => 0,
            MessageContentDupType::SendViaBot => 1,
            MessageContentDupType::SendQuickReply => 2,
            MessageContentDupType::Forward => 3,
            MessageContentDupType::Copy => 4,
            MessageContentDupType::ServerCopy => 5,
        }
    }

    #[allow(dead_code)] // the reverse of as_i32; the store/parse consumers arrive with M.10
    fn from_i32(value: i32) -> MessageContentDupType {
        match value {
            1 => MessageContentDupType::SendViaBot,
            2 => MessageContentDupType::SendQuickReply,
            3 => MessageContentDupType::Forward,
            4 => MessageContentDupType::Copy,
            5 => MessageContentDupType::ServerCopy,
            _ => MessageContentDupType::Send,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_send_table() {
        assert!(MessageContentDupType::Send.is_send());
        assert!(MessageContentDupType::SendViaBot.is_send());
        assert!(MessageContentDupType::SendQuickReply.is_send());
        assert!(!MessageContentDupType::Forward.is_send());
        assert!(!MessageContentDupType::Copy.is_send());
        assert!(!MessageContentDupType::ServerCopy.is_send());
    }

    #[test]
    fn int32_mirror_round_trips() {
        for dup_type in [
            MessageContentDupType::Send,
            MessageContentDupType::SendViaBot,
            MessageContentDupType::SendQuickReply,
            MessageContentDupType::Forward,
            MessageContentDupType::Copy,
            MessageContentDupType::ServerCopy,
        ] {
            assert_eq!(MessageContentDupType::from_i32(dup_type.as_i32()), dup_type);
        }
    }
}
