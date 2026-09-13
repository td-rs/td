//! Port of `td/mtproto/MessageId.h`: the MTProto message identifier.

use std::fmt;

/// `td::mtproto::MessageId`: an MTProto `msg_id` — approximately
/// `server_time * 2^32`, always divisible by 4, even for client messages and
/// odd for server messages.
///
/// The C++ class deletes implicit conversions from integers; the Rust type is
/// a newtype with an explicit [`MessageId::new`] for the same reason.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MessageId(u64);

impl MessageId {
    pub const fn new(message_id: u64) -> Self {
        Self(message_id)
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

impl From<u64> for MessageId {
    fn from(message_id: u64) -> Self {
        Self::new(message_id)
    }
}

/// `operator<<(StringBuilder&, MessageId)`: `message <hex>` (no `0x` prefix).
impl fmt::Display for MessageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "message {:x}", self.0)
    }
}
