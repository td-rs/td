//! `td::SecretChatId` + the `SecretChatEvent` logevent records
//! (SecretChatId.h + logevent/SecretChatEvent.h, M.50).
//!
//! The decrypted-message-layer halves ride the env seams ([blocked:
//! tde2e] — the e2e codegen lives in the tde2e crate and the actor plane
//! lands with it); the record framing is byte-exact.

use crate::store_parse::{self, Parser};
use tdmtproto::telegram_api;

/// `td::SecretChatState` (SecretChatId.h:13).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SecretChatState {
    #[default]
    Unknown = -1,
    Waiting = 0,
    Active = 1,
    Closed = 2,
}

impl SecretChatState {
    /// `store` — the `enum class : int32` slot.
    pub fn store(self, out: &mut Vec<u8>) {
        store_parse::store_i32(out, self as i32);
    }

    /// `parse`.
    pub fn parse(parser: &mut Parser) -> std::result::Result<SecretChatState, &'static str> {
        Ok(match parser.i32_()? {
            -1 => SecretChatState::Unknown,
            0 => SecretChatState::Waiting,
            1 => SecretChatState::Active,
            2 => SecretChatState::Closed,
            _ => return Err("invalid SecretChatState"),
        })
    }
}

/// `td::SecretChatId` (SecretChatId.h:15-45).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SecretChatId(i32);

impl SecretChatId {
    pub const fn new(id: i32) -> SecretChatId {
        SecretChatId(id)
    }

    /// `SecretChatId::is_valid` (SecretChatId.h:20-22).
    pub const fn is_valid(self) -> bool {
        self.0 != 0
    }

    /// `SecretChatId::get` (SecretChatId.h:23-25).
    pub const fn get(self) -> i32 {
        self.0
    }

    /// `SecretChatId::store` (SecretChatId.h:33-35).
    pub fn store(self, out: &mut Vec<u8>) {
        store_parse::store_i32(out, self.0);
    }

    /// `SecretChatId::parse` (SecretChatId.h:36-38).
    pub fn parse(parser: &mut Parser) -> std::result::Result<SecretChatId, &'static str> {
        Ok(SecretChatId::new(parser.i32_()?))
    }
}

impl std::fmt::Display for SecretChatId {
    /// `operator<<` (SecretChatId.h:44-46).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "secret chat {}", self.0)
    }
}

/// `EncryptedInputFile` (SecretChatEvent.h:83-160).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EncryptedInputFile {
    type_: i32,
    id: i64,
    access_hash: i64,
    parts: i32,
    key_fingerprint: i32,
}

/// `EncryptedInputFile::MAGIC` (SecretChatEvent.h:84).
const ENCRYPTED_INPUT_FILE_MAGIC: i32 = 0x4328d38au32 as i32;
/// `EncryptedInputFile::Type` (SecretChatEvent.h:85).
const FILE_TYPE_EMPTY: i32 = 0;
const FILE_TYPE_UPLOADED: i32 = 1;
const FILE_TYPE_BIG_UPLOADED: i32 = 2;
const FILE_TYPE_LOCATION: i32 = 3;

impl EncryptedInputFile {
    /// `from_input_encrypted_file` (SecretChatEvent.h:114-142).
    pub fn from_input_encrypted_file(
        from: Option<&telegram_api::InputEncryptedFile>,
    ) -> EncryptedInputFile {
        let Some(from) = from else {
            return EncryptedInputFile::default();
        };
        match from {
            telegram_api::InputEncryptedFile::inputEncryptedFileEmpty(_) => EncryptedInputFile {
                type_: FILE_TYPE_EMPTY,
                ..EncryptedInputFile::default()
            },
            telegram_api::InputEncryptedFile::inputEncryptedFileUploaded(uploaded) => {
                EncryptedInputFile {
                    type_: FILE_TYPE_UPLOADED,
                    id: uploaded.id,
                    parts: uploaded.parts,
                    key_fingerprint: uploaded.key_fingerprint,
                    ..EncryptedInputFile::default()
                }
            }
            telegram_api::InputEncryptedFile::inputEncryptedFileBigUploaded(uploaded) => {
                EncryptedInputFile {
                    type_: FILE_TYPE_BIG_UPLOADED,
                    id: uploaded.id,
                    parts: uploaded.parts,
                    key_fingerprint: uploaded.key_fingerprint,
                    ..EncryptedInputFile::default()
                }
            }
            telegram_api::InputEncryptedFile::inputEncryptedFile(uploaded) => EncryptedInputFile {
                type_: FILE_TYPE_LOCATION,
                id: uploaded.id,
                access_hash: uploaded.access_hash,
                ..EncryptedInputFile::default()
            },
        }
    }

    /// `EncryptedInputFile::empty` (SecretChatEvent.h:109-111).
    pub fn empty(&self) -> bool {
        self.type_ == FILE_TYPE_EMPTY
    }

    /// `store` (SecretChatEvent.h:91-97).
    pub fn store(self, out: &mut Vec<u8>) {
        store_parse::store_i32(out, ENCRYPTED_INPUT_FILE_MAGIC);
        store_parse::store_i32(out, self.type_);
        store_parse::store_i64(out, self.id);
        store_parse::store_i64(out, self.access_hash);
        store_parse::store_i32(out, self.parts);
        store_parse::store_i32(out, self.key_fingerprint);
    }

    /// `parse` (SecretChatEvent.h:100-113).
    pub fn parse(parser: &mut Parser) -> std::result::Result<EncryptedInputFile, &'static str> {
        let stored_magic = parser.i32_()?;
        let result = EncryptedInputFile {
            type_: parser.i32_()?,
            id: parser.i64_()?,
            access_hash: parser.i64_()?,
            parts: parser.i32_()?,
            key_fingerprint: parser.i32_()?,
        };
        if stored_magic != ENCRYPTED_INPUT_FILE_MAGIC {
            return Err("EncryptedInputFile magic mismatch");
        }
        Ok(result)
    }
}

/// `SecretChatEvent::Type` (SecretChatEvent.h:141-147).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecretChatEventType {
    InboundSecretMessage = 1,
    OutboundSecretMessage = 2,
    CloseSecretChat = 3,
    CreateSecretChat = 4,
}

impl SecretChatEventType {
    /// `store` — the `enum class : int32` magic slot.
    pub fn store(self, out: &mut Vec<u8>) {
        store_parse::store_i32(out, self as i32);
    }

    /// `parse`.
    pub fn parse(parser: &mut Parser) -> std::result::Result<SecretChatEventType, &'static str> {
        Ok(match parser.i32_()? {
            1 => SecretChatEventType::InboundSecretMessage,
            2 => SecretChatEventType::OutboundSecretMessage,
            3 => SecretChatEventType::CloseSecretChat,
            4 => SecretChatEventType::CreateSecretChat,
            _ => return Err("unknown SecretChatEvent type"),
        })
    }
}

/// `SecretChatEvent::version` (SecretChatEvent.h:149-151).
pub const SECRET_CHAT_EVENT_VERSION: i32 = 4;

/// The `SecretChatEvent` framing: version, the type magic, then the
/// record (the C++ `StorerImpl` of SecretChatEvent.h:116-136).
pub fn store_event_prefix(event_type: SecretChatEventType, out: &mut Vec<u8>) {
    store_parse::store_i32(out, SECRET_CHAT_EVENT_VERSION);
    event_type.store(out);
}

/// `CloseSecretChat` (SecretChatEvent.h) — the close record.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CloseSecretChat {
    pub chat_id: i32,
}

impl CloseSecretChat {
    /// `store` (SecretChatEvent.h) — chat_id only.
    pub fn store(&self, out: &mut Vec<u8>) {
        store_parse::store_i32(out, self.chat_id);
    }

    /// `parse`.
    pub fn parse(parser: &mut Parser) -> std::result::Result<CloseSecretChat, &'static str> {
        Ok(CloseSecretChat {
            chat_id: parser.i32_()?,
        })
    }
}

/// `CreateSecretChat` (SecretChatEvent.h) — the create record: chat id +
/// random id.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CreateSecretChat {
    pub chat_id: i32,
    pub random_id: i64,
}

impl CreateSecretChat {
    /// `store` (SecretChatEvent.h).
    pub fn store(&self, out: &mut Vec<u8>) {
        store_parse::store_i32(out, self.chat_id);
        store_parse::store_i64(out, self.random_id);
    }

    /// `parse`.
    pub fn parse(parser: &mut Parser) -> std::result::Result<CreateSecretChat, &'static str> {
        Ok(CreateSecretChat {
            chat_id: parser.i32_()?,
            random_id: parser.i64_()?,
        })
    }
}

/// `InboundSecretMessage` (SecretChatEvent.h:161-232) — the decrypted
/// layer rides the env seam ([blocked: tde2e]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InboundSecretMessage {
    pub chat_id: i32,
    pub date: i32,
    pub auth_key_id: u64,
    pub message_id: i32,
    pub my_in_seq_no: i32,
    pub my_out_seq_no: i32,
    pub his_in_seq_no: i32,
    pub file: Option<EncryptedInputFile>,
    pub is_pending: bool,
}

impl InboundSecretMessage {
    /// `store` (SecretChatEvent.h:180-205) — the decrypted layer rides
    /// the env seam.
    pub fn store(&self, env: &impl SecretChatEnv, out: &mut Vec<u8>) {
        let has_encrypted_file = self.file.is_some();
        store_parse::store_flags(out, [has_encrypted_file, self.is_pending, true]);
        store_parse::store_i32(out, self.chat_id);
        store_parse::store_i32(out, self.date);
        env.store_decrypted_message_layer(out);
        store_parse::store_i64(out, self.auth_key_id as i64);
        store_parse::store_i32(out, self.message_id);
        store_parse::store_i32(out, self.my_in_seq_no);
        store_parse::store_i32(out, self.my_out_seq_no);
        store_parse::store_i32(out, self.his_in_seq_no);
        if let Some(file) = self.file {
            file.store(out);
        }
    }

    /// `parse` (SecretChatEvent.h:206-230).
    pub fn parse(
        env: &impl SecretChatEnv,
        parser: &mut Parser,
    ) -> std::result::Result<InboundSecretMessage, &'static str> {
        let [has_encrypted_file, is_pending, no_qts] = parser.parse_flags()?;
        if !no_qts {
            let _legacy_qts = parser.i32_()?;
        }
        let mut result = InboundSecretMessage {
            chat_id: parser.i32_()?,
            date: parser.i32_()?,
            is_pending,
            ..InboundSecretMessage::default()
        };
        env.parse_decrypted_message_layer(parser)?;
        result.auth_key_id = parser.i64_()? as u64;
        result.message_id = parser.i32_()?;
        result.my_in_seq_no = parser.i32_()?;
        result.my_out_seq_no = parser.i32_()?;
        result.his_in_seq_no = parser.i32_()?;
        if has_encrypted_file {
            result.file = Some(EncryptedInputFile::parse(parser)?);
        }
        Ok(result)
    }
}

/// `OutboundSecretMessage` (SecretChatEvent.h:233-...) — the encrypted
/// message buffer and the decrypted layer ride the env seams.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OutboundSecretMessage {
    pub chat_id: i32,
    pub random_id: i64,
    pub file: EncryptedInputFile,
}

impl OutboundSecretMessage {
    /// `store` (SecretChatEvent.h) — the decrypted layer rides the env
    /// seam; the encrypted buffer is skipped like the C++.
    pub fn store(&self, env: &impl SecretChatEnv, out: &mut Vec<u8>) {
        let has_file = !self.file.empty();
        store_parse::store_flags(out, [has_file]);
        store_parse::store_i32(out, self.chat_id);
        store_parse::store_i64(out, self.random_id);
        env.store_decrypted_message_layer(out);
        if has_file {
            self.file.store(out);
        }
    }

    /// `parse`.
    pub fn parse(
        env: &impl SecretChatEnv,
        parser: &mut Parser,
    ) -> std::result::Result<OutboundSecretMessage, &'static str> {
        let [has_file] = parser.parse_flags()?;
        let mut result = OutboundSecretMessage {
            chat_id: parser.i32_()?,
            random_id: parser.i64_()?,
            ..OutboundSecretMessage::default()
        };
        env.parse_decrypted_message_layer(parser)?;
        if has_file {
            result.file = EncryptedInputFile::parse(parser)?;
        }
        Ok(result)
    }
}

/// The `G()` slice the secret-chat value plane reads through — the
/// decrypted-message-layer halves are [blocked: tde2e].
pub trait SecretChatEnv {
    fn store_decrypted_message_layer(&self, out: &mut Vec<u8>) {
        tdutils::log_info!("store_decrypted_message_layer — [blocked: tde2e]");
        store_parse::store_flags(out, []);
    }
    fn parse_decrypted_message_layer(
        &self,
        parser: &mut Parser,
    ) -> std::result::Result<(), &'static str> {
        tdutils::log_info!("parse_decrypted_message_layer — [blocked: tde2e]");
        parser.parse_flags::<0>()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoopEnv;

    impl SecretChatEnv for NoopEnv {}

    #[test]
    fn secret_chat_id_store_parse_round_trip() {
        let value = SecretChatId::new(123456);
        let mut out = Vec::new();
        value.store(&mut out);
        let mut parser = Parser::new(&out);
        assert_eq!(SecretChatId::parse(&mut parser).unwrap(), value);
        assert!(value.is_valid());
        assert!(!SecretChatId::default().is_valid());
    }

    #[test]
    fn encrypted_input_file_round_trip_and_magic() {
        let value = EncryptedInputFile {
            type_: FILE_TYPE_UPLOADED,
            id: 42,
            parts: 3,
            key_fingerprint: 7,
            ..EncryptedInputFile::default()
        };
        let mut out = Vec::new();
        value.store(&mut out);
        let mut parser = Parser::new(&out);
        assert_eq!(EncryptedInputFile::parse(&mut parser).unwrap(), value);
    }

    #[test]
    fn close_and_create_records_round_trip() {
        let value = CloseSecretChat { chat_id: 9 };
        let mut out = Vec::new();
        value.store(&mut out);
        let mut parser = Parser::new(&out);
        assert_eq!(CloseSecretChat::parse(&mut parser).unwrap(), value);

        let value = CreateSecretChat {
            chat_id: 9,
            random_id: 123456789,
        };
        let mut out = Vec::new();
        value.store(&mut out);
        let mut parser = Parser::new(&out);
        assert_eq!(CreateSecretChat::parse(&mut parser).unwrap(), value);
    }

    #[test]
    fn inbound_record_round_trip_over_the_env_seam() {
        let value = InboundSecretMessage {
            chat_id: 5,
            date: 1000,
            auth_key_id: 0xABCD,
            message_id: 3,
            my_in_seq_no: 1,
            my_out_seq_no: 2,
            his_in_seq_no: 3,
            file: None,
            is_pending: false,
        };
        let env = NoopEnv;
        let mut out = Vec::new();
        value.store(&env, &mut out);
        let mut parser = Parser::new(&out);
        assert_eq!(
            InboundSecretMessage::parse(&env, &mut parser).unwrap(),
            value
        );
    }
}
