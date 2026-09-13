//! Port of `td/mtproto/PacketInfo.h`: metadata of one MTProto packet.

use crate::message_id::MessageId;

/// `td::mtproto::PacketInfo`: everything the crypto layer reads from or
/// writes into a packet header.
#[derive(Debug, Default)]
pub struct PacketInfo {
    pub type_: PacketType,
    pub message_ack: u32,
    pub salt: u64,
    pub session_id: u64,
    pub message_id: MessageId,
    pub seq_no: i32,
    pub version: i32,
    pub no_crypto_flag: bool,
    pub is_creator: bool,
    pub check_mod4: bool,
    pub use_random_padding: bool,
}

/// `PacketInfo::enum { Common, EndToEnd }`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum PacketType {
    #[default]
    Common,
    EndToEnd,
}

impl PacketInfo {
    pub fn new() -> Self {
        Self {
            version: 1,
            check_mod4: true,
            ..Self::default()
        }
    }
}
