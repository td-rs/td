//! Port of `td/telegram/MessageEntity.{h,cpp,hpp}`: the [`MessageEntity`]
//! model, the [`FormattedText`] pair, entity ordering/fixup machinery, the
//! text matchers (mentions, URLs, bank cards, ...), the Markdown
//! v1/v2/v3 and HTML parsers, and the td_api/telegram_api/secret_api
//! converters.
//!
//! Slice map (RUST_REWRITE_PLAN_MESSAGEENTITY_M8.md):
//! - M.8.1 — model + td_api out-converters
//! - M.8.2 — input converters + `fix_entities`
//! - M.8.3/M.8.4 — matchers
//! - M.8.5 — validation, ordering, merging, `find_entities`
//! - M.8.6..M.8.9 — Markdown v1/v2/v3 + HTML parsers
//!
//! Offset/length units are UTF-16 code units everywhere (the C++ uses
//! `td::utf16_length` etc.); helpers live in `tdutils::utf8`.

use crate::chat_manager::{ChannelId, ChatId};
use crate::emoji_status::CustomEmojiId;
use crate::formatted_date::FormattedDate;
use crate::td_api;
use crate::user_manager::{SecretChatId, UserId};
use tdutils::status::Status;

/// The `const UserManager *user_manager` parameter of the C++ converters.
/// The C++ passes `nullptr` when the raw user id must be emitted without
/// touching the manager; the port models that as `&()`.
pub trait UserManagerLike {
    /// `UserManager::get_user_id_object` — returns the object-facing user
    /// id (the port keeps it the raw id) and may emit unknown-user side
    /// effects when backed by the real manager.
    fn resolve_user_id_object(&mut self, user_id: UserId, source: &str) -> i64;

    /// `UserManager::get_input_user`, validation half: the `TRY_STATUS`
    /// sites only consume the error. `None` (`nullptr`) skips it entirely.
    fn ensure_input_user(&mut self, user_id: UserId) -> Result<(), Status>;

    /// `UserManager::have_min_user`.
    fn have_min_user(&self, user_id: UserId) -> bool;

    /// `UserManager::get_input_user_force` — the C++ CHECKs
    /// `user_manager != nullptr` before this call.
    fn get_input_user_force(&mut self, user_id: UserId) -> tdmtproto::telegram_api::InputUser;

    /// `UserManager::is_user_bot` — the `need_always_skip_bot_commands`
    /// probe; `()` knows no users, so nothing is a bot.
    fn is_user_bot(&self, _user_id: UserId) -> bool {
        false
    }

    /// `UserManager::get_secret_chat_user_id` — `()` knows no secret chats.
    fn get_secret_chat_user_id(&self, _secret_chat_id: SecretChatId) -> UserId {
        UserId::default()
    }

    /// `UserManager::get_secret_chat_layer` — `()` knows no secret chats.
    fn get_secret_chat_layer(&self, _secret_chat_id: SecretChatId) -> i32 {
        0
    }

    /// `UserManager::get_replies_bot_user_id` — production value; the
    /// test-dc value is selected by the real manager implementation.
    fn get_replies_bot_user_id() -> UserId {
        UserId::new(1271266957)
    }

    /// `UserManager::get_verification_codes_bot_user_id`.
    fn get_verification_codes_bot_user_id() -> UserId {
        UserId::new(489000)
    }
}

/// The C++ `user_manager == nullptr` case: raw ids, no validation, no
/// min-user knowledge (server MentionNames are skipped in that mode).
impl UserManagerLike for () {
    fn resolve_user_id_object(&mut self, user_id: UserId, _source: &str) -> i64 {
        user_id.get()
    }

    fn ensure_input_user(&mut self, _user_id: UserId) -> Result<(), Status> {
        Ok(())
    }

    fn have_min_user(&self, _user_id: UserId) -> bool {
        false
    }

    fn get_input_user_force(&mut self, _user_id: UserId) -> tdmtproto::telegram_api::InputUser {
        unreachable!("the C++ CHECK(user_manager != nullptr) fired")
    }
}

/// Minimal `DialogId` (DialogId.h) — the full port lands with the dialog
/// machinery; this carries only the identity the entity rules consume.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DialogId {
    User(UserId),
    SecretChat(SecretChatId),
    Chat(ChatId),
    Channel(ChannelId),
    None,
}

impl DialogId {
    /// `DialogId::is_valid` — `None` models an invalid id; the inner-id
    /// validity checks land with the full DialogId port (every branch the
    /// entity rules take agrees with the C++ either way).
    pub fn is_valid(self) -> bool {
        !matches!(self, DialogId::None)
    }
}

/// `td::MessageEntity::Type` (MessageEntity.h:35-60). `Size` is the C++
/// default sentinel; it is only ever observable through corrupted
/// `store`/`parse` inputs and no converter accepts it (the C++ hits
/// `UNREACHABLE()`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(i32)]
pub enum Type {
    Mention = 0,
    Hashtag = 1,
    BotCommand = 2,
    Url = 3,
    EmailAddress = 4,
    Bold = 5,
    Italic = 6,
    Code = 7,
    Pre = 8,
    PreCode = 9,
    TextUrl = 10,
    MentionName = 11,
    Cashtag = 12,
    PhoneNumber = 13,
    Underline = 14,
    Strikethrough = 15,
    BlockQuote = 16,
    BankCardNumber = 17,
    MediaTimestamp = 18,
    Spoiler = 19,
    CustomEmoji = 20,
    ExpandableBlockQuote = 21,
    FormattedDate = 22,
}

impl Type {
    /// The `Type::Size` sentinel value.
    pub const SIZE: i32 = 23;

    /// From the stored `int32` discriminant; out-of-range values become
    /// the `Size` sentinel (there is no `Size` variant to hold).
    pub fn from_i32(value: i32) -> Option<Type> {
        match value {
            0 => Some(Type::Mention),
            1 => Some(Type::Hashtag),
            2 => Some(Type::BotCommand),
            3 => Some(Type::Url),
            4 => Some(Type::EmailAddress),
            5 => Some(Type::Bold),
            6 => Some(Type::Italic),
            7 => Some(Type::Code),
            8 => Some(Type::Pre),
            9 => Some(Type::PreCode),
            10 => Some(Type::TextUrl),
            11 => Some(Type::MentionName),
            12 => Some(Type::Cashtag),
            13 => Some(Type::PhoneNumber),
            14 => Some(Type::Underline),
            15 => Some(Type::Strikethrough),
            16 => Some(Type::BlockQuote),
            17 => Some(Type::BankCardNumber),
            18 => Some(Type::MediaTimestamp),
            19 => Some(Type::Spoiler),
            20 => Some(Type::CustomEmoji),
            21 => Some(Type::ExpandableBlockQuote),
            22 => Some(Type::FormattedDate),
            _ => None,
        }
    }
}

impl std::fmt::Display for Type {
    /// `operator<<(StringBuilder, MessageEntity::Type)` (MessageEntity.cpp:66-119).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Type::Mention => "Mention",
            Type::Hashtag => "Hashtag",
            Type::BotCommand => "BotCommand",
            Type::Url => "Url",
            Type::EmailAddress => "EmailAddress",
            Type::Bold => "Bold",
            Type::Italic => "Italic",
            Type::Underline => "Underline",
            Type::Strikethrough => "Strikethrough",
            Type::BlockQuote => "BlockQuote",
            Type::Code => "Code",
            Type::Pre => "Pre",
            Type::PreCode => "PreCode",
            Type::TextUrl => "TextUrl",
            Type::MentionName => "MentionName",
            Type::Cashtag => "Cashtag",
            Type::PhoneNumber => "PhoneNumber",
            Type::BankCardNumber => "BankCardNumber",
            Type::MediaTimestamp => "MediaTimestamp",
            Type::Spoiler => "Spoiler",
            Type::CustomEmoji => "CustomEmoji",
            Type::ExpandableBlockQuote => "ExpandableBlockQuote",
            Type::FormattedDate => "Date",
        };
        f.write_str(name)
    }
}

/// `td::MessageEntity` (MessageEntity.h:33-125).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessageEntity {
    /// The C++ default is `Type::Size`; entities in flight always carry a
    /// real type, so the port models the sentinel as `None`.
    pub r#type: Option<Type>,
    pub offset: i32,
    pub length: i32,
    pub media_timestamp: i32,
    pub date: FormattedDate,
    pub argument: String,
    pub user_id: UserId,
    pub custom_emoji_id: CustomEmojiId,
}

impl Default for MessageEntity {
    /// The C++ member defaults (MessageEntity.h:61-70): `Type::Size`,
    /// offset = length = media_timestamp = -1.
    fn default() -> MessageEntity {
        MessageEntity {
            r#type: None,
            offset: -1,
            length: -1,
            media_timestamp: -1,
            date: FormattedDate::default(),
            argument: String::new(),
            user_id: UserId::default(),
            custom_emoji_id: CustomEmojiId::default(),
        }
    }
}

impl MessageEntity {
    /// `MessageEntity(Type, int32, int32, string argument = "")`.
    pub fn new(r#type: Type, offset: i32, length: i32) -> MessageEntity {
        MessageEntity {
            r#type: Some(r#type),
            offset,
            length,
            ..MessageEntity::default()
        }
    }

    /// The `(Type, int32, int32, string)` overload.
    pub fn with_argument(
        r#type: Type,
        offset: i32,
        length: i32,
        argument: String,
    ) -> MessageEntity {
        MessageEntity {
            r#type: Some(r#type),
            offset,
            length,
            argument,
            ..MessageEntity::default()
        }
    }

    /// `MessageEntity(int32, int32, UserId)` — a MentionName.
    pub fn mention_name(offset: i32, length: i32, user_id: UserId) -> MessageEntity {
        MessageEntity {
            r#type: Some(Type::MentionName),
            offset,
            length,
            user_id,
            ..MessageEntity::default()
        }
    }

    /// `MessageEntity(Type, int32, int32, int32 media_timestamp)`.
    pub fn media_timestamp(offset: i32, length: i32, media_timestamp: i32) -> MessageEntity {
        MessageEntity {
            r#type: Some(Type::MediaTimestamp),
            offset,
            length,
            media_timestamp,
            ..MessageEntity::default()
        }
    }

    /// `MessageEntity(Type, int32, int32, CustomEmojiId)`.
    pub fn custom_emoji(offset: i32, length: i32, custom_emoji_id: CustomEmojiId) -> MessageEntity {
        MessageEntity {
            r#type: Some(Type::CustomEmoji),
            offset,
            length,
            custom_emoji_id,
            ..MessageEntity::default()
        }
    }

    /// `MessageEntity(Type, int32, int32, FormattedDate)`.
    pub fn formatted_date(offset: i32, length: i32, date: FormattedDate) -> MessageEntity {
        MessageEntity {
            r#type: Some(Type::FormattedDate),
            offset,
            length,
            date,
            ..MessageEntity::default()
        }
    }

    /// `MessageEntity::get_type_priority` (MessageEntity.cpp:38-64).
    pub fn type_priority(r#type: Type) -> i32 {
        const PRIORITIES: [i32; 23] = [
            50, // Mention
            50, // Hashtag
            50, // BotCommand
            50, // Url
            50, // EmailAddress
            90, // Bold
            91, // Italic
            20, // Code
            11, // Pre
            10, // PreCode
            49, // TextUrl
            49, // MentionName
            50, // Cashtag
            50, // PhoneNumber
            92, // Underline
            93, // Strikethrough
            0,  // BlockQuote
            50, // BankCardNumber
            50, // MediaTimestamp
            94, // Spoiler
            99, // CustomEmoji
            0,  // ExpandableBlockQuote
            30, // FormattedDate
        ];
        PRIORITIES[r#type as usize]
    }

    /// `MessageEntity::get_text_entity_type_object`
    /// (MessageEntity.cpp:142-198).
    pub fn get_text_entity_type_object<U: UserManagerLike + ?Sized>(
        &self,
        user_manager: Option<&mut U>,
    ) -> td_api::TextEntityType {
        use td_api::*;
        let default_case = || -> td_api::TextEntityType {
            // UNREACHABLE() in C++ (MessageEntity.cpp:196); a `Size`-typed
            // entity must never reach the converters.
            unreachable!("get_text_entity_type_object on an invalid entity type");
        };
        let Some(r#type) = self.r#type else {
            return default_case();
        };
        match r#type {
            Type::Mention => TextEntityType::textEntityTypeMention(textEntityTypeMention {}),
            Type::Hashtag => TextEntityType::textEntityTypeHashtag(textEntityTypeHashtag {}),
            Type::BotCommand => {
                TextEntityType::textEntityTypeBotCommand(textEntityTypeBotCommand {})
            }
            Type::Url => TextEntityType::textEntityTypeUrl(textEntityTypeUrl {}),
            Type::EmailAddress => {
                TextEntityType::textEntityTypeEmailAddress(textEntityTypeEmailAddress {})
            }
            Type::Bold => TextEntityType::textEntityTypeBold(textEntityTypeBold {}),
            Type::Italic => TextEntityType::textEntityTypeItalic(textEntityTypeItalic {}),
            Type::Underline => TextEntityType::textEntityTypeUnderline(textEntityTypeUnderline {}),
            Type::Strikethrough => {
                TextEntityType::textEntityTypeStrikethrough(textEntityTypeStrikethrough {})
            }
            Type::BlockQuote => {
                TextEntityType::textEntityTypeBlockQuote(textEntityTypeBlockQuote {})
            }
            Type::Code => TextEntityType::textEntityTypeCode(textEntityTypeCode {}),
            Type::Pre => TextEntityType::textEntityTypePre(textEntityTypePre {}),
            Type::PreCode => TextEntityType::textEntityTypePreCode(textEntityTypePreCode {
                language: self.argument.clone(),
            }),
            Type::TextUrl => TextEntityType::textEntityTypeTextUrl(textEntityTypeTextUrl {
                url: self.argument.clone(),
            }),
            Type::MentionName => {
                let user_id = match user_manager {
                    Some(user_manager) => user_manager
                        .resolve_user_id_object(self.user_id, "textEntityTypeMentionName"),
                    None => self.user_id.get(),
                };
                TextEntityType::textEntityTypeMentionName(textEntityTypeMentionName { user_id })
            }
            Type::Cashtag => TextEntityType::textEntityTypeCashtag(textEntityTypeCashtag {}),
            Type::PhoneNumber => {
                TextEntityType::textEntityTypePhoneNumber(textEntityTypePhoneNumber {})
            }
            Type::BankCardNumber => {
                TextEntityType::textEntityTypeBankCardNumber(textEntityTypeBankCardNumber {})
            }
            Type::MediaTimestamp => {
                TextEntityType::textEntityTypeMediaTimestamp(textEntityTypeMediaTimestamp {
                    media_timestamp: self.media_timestamp,
                })
            }
            Type::Spoiler => TextEntityType::textEntityTypeSpoiler(textEntityTypeSpoiler {}),
            Type::CustomEmoji => {
                TextEntityType::textEntityTypeCustomEmoji(textEntityTypeCustomEmoji {
                    custom_emoji_id: self.custom_emoji_id.get(),
                })
            }
            Type::ExpandableBlockQuote => TextEntityType::textEntityTypeExpandableBlockQuote(
                textEntityTypeExpandableBlockQuote {},
            ),
            Type::FormattedDate => {
                // The C++ passes `get_date_time_formatting_type_object()`,
                // which is null when no formatting was requested; since the
                // M.10 union the generated td_api carries the `Option`
                // through, so the null is passed on exactly like the C++
                // (the S09-era default-Relative stand-in is gone).
                let formatting_type = self.date.get_date_time_formatting_type_object();
                TextEntityType::textEntityTypeDateTime(textEntityTypeDateTime {
                    unix_time: self.date.get_date(),
                    formatting_type,
                })
            }
        }
    }

    /// `MessageEntity::get_text_entity_object` (MessageEntity.cpp:200-202).
    pub fn get_text_entity_object<U: UserManagerLike + ?Sized>(
        &self,
        user_manager: Option<&mut U>,
    ) -> td_api::TextEntity {
        td_api::TextEntity {
            offset: self.offset,
            length: self.length,
            r#type: Some(self.get_text_entity_type_object(user_manager)),
        }
    }

    /// `MessageEntity::store` (MessageEntity.hpp:17-37).
    pub fn store(&self, out: &mut Vec<u8>) {
        let sp = &mut crate::store_parse::store_i32;
        sp(out, self.r#type.map_or(Type::SIZE, |t| t as i32));
        sp(out, self.offset);
        sp(out, self.length);
        let r#type = self.r#type;
        if r#type == Some(Type::PreCode) || r#type == Some(Type::TextUrl) {
            crate::store_parse::store_string(out, &self.argument);
        }
        if r#type == Some(Type::MentionName) {
            // `store(UserId)` stores the wrapped int64.
            crate::store_parse::store_i64(out, self.user_id.get());
        }
        if r#type == Some(Type::MediaTimestamp) {
            sp(out, self.media_timestamp);
        }
        if r#type == Some(Type::CustomEmoji) {
            crate::store_parse::store_i64(out, self.custom_emoji_id.get());
        }
        if r#type == Some(Type::FormattedDate) {
            self.date.store(out);
        }
    }

    /// `MessageEntity::parse` (MessageEntity.hpp:39-60).
    pub fn parse(input: &mut crate::store_parse::Parser) -> Result<MessageEntity, &'static str> {
        let type_raw = input.i32_()?;
        let offset = input.i32_()?;
        let length = input.i32_()?;
        let mut entity = MessageEntity {
            r#type: Type::from_i32(type_raw),
            offset,
            length,
            ..MessageEntity::default()
        };
        if type_raw == Type::PreCode as i32 || type_raw == Type::TextUrl as i32 {
            entity.argument = input.string()?;
        }
        if type_raw == Type::MentionName as i32 {
            entity.user_id = UserId::new(input.i64_()?);
        }
        if type_raw == Type::MediaTimestamp as i32 {
            entity.media_timestamp = input.i32_()?;
        }
        if type_raw == Type::CustomEmoji as i32 {
            entity.custom_emoji_id = CustomEmojiId::new(input.i64_()?);
        }
        if type_raw == Type::FormattedDate as i32 {
            entity.date = FormattedDate::parse(input)?;
        }
        Ok(entity)
    }

    /// The C++ `operator<` (MessageEntity.h:99-109): by offset, then by
    /// descending length, then by type priority.
    pub fn cmp_key(&self) -> (i32, std::cmp::Reverse<i32>, i32) {
        (
            self.offset,
            std::cmp::Reverse(self.length),
            self.r#type.map_or(i32::MAX, MessageEntity::type_priority),
        )
    }
}

/// The C++ `operator<` over entities.
impl Ord for MessageEntity {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.cmp_key().cmp(&other.cmp_key())
    }
}

impl PartialOrd for MessageEntity {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl std::fmt::Display for MessageEntity {
    /// `operator<<(StringBuilder, MessageEntity)` (MessageEntity.cpp:121-140).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "[{}, offset = {}, length = {}",
            self.r#type
                .map_or("Impossible".to_owned(), |t| t.to_string()),
            self.offset,
            self.length
        )?;
        if self.media_timestamp >= 0 {
            write!(f, ", media_timestamp = \"{}\"", self.media_timestamp)?;
        }
        if !self.argument.is_empty() {
            write!(f, ", argument = \"{}\"", self.argument)?;
        }
        if self.user_id.is_valid() {
            write!(f, ", user {}", self.user_id.get())?;
        }
        if self.custom_emoji_id.is_valid() {
            write!(f, ", custom emoji {}", self.custom_emoji_id.get())?;
        }
        if self.date.is_valid() {
            write!(f, ", at {}", self.date)?;
        }
        f.write_str("]")
    }
}

/// `td::FormattedText` (MessageEntity.h:131-140).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FormattedText {
    pub text: String,
    pub entities: Vec<MessageEntity>,
}

impl FormattedText {
    /// `FormattedText::store` (MessageEntity.hpp:62-66).
    pub fn store(&self, out: &mut Vec<u8>) {
        crate::store_parse::store_string(out, &self.text);
        crate::store_parse::store_i32(out, self.entities.len() as i32);
        for entity in &self.entities {
            entity.store(out);
        }
    }

    /// `FormattedText::parse` (MessageEntity.hpp:68-73): the C++ runs
    /// `remove_empty_entities` on the result (M.8.5).
    pub fn parse(input: &mut crate::store_parse::Parser) -> Result<FormattedText, &'static str> {
        let text = input.string()?;
        let len = input.i32_()?;
        if len < 0 {
            return Err("Wrong vector length");
        }
        let mut entities = Vec::with_capacity(len as usize);
        for _ in 0..len {
            entities.push(MessageEntity::parse(input)?);
        }
        remove_empty_entities(&mut entities);
        Ok(FormattedText { text, entities })
    }
}

impl std::fmt::Display for FormattedText {
    /// `operator<<(StringBuilder, FormattedText)` (MessageEntity.cpp:206-208).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "\"{}\" with entities ", self.text)?;
        // vector<MessageEntity> prints "[e1, e2, ...]".
        f.write_str("[")?;
        for (i, entity) in self.entities.iter().enumerate() {
            if i != 0 {
                f.write_str(", ")?;
            }
            write!(f, "{entity}")?;
        }
        f.write_str("]")
    }
}

/// `get_text_entities_object` (MessageEntity.cpp:204-224): the shared
/// td_api out-converter with the td_api knobs
/// (`skip_bot_commands`, `max_media_timestamp`).
pub fn get_text_entities_object<U: UserManagerLike + ?Sized>(
    mut user_manager: Option<&mut U>,
    entities: &[MessageEntity],
    skip_bot_commands: bool,
    max_media_timestamp: i32,
) -> Vec<td_api::TextEntity> {
    let mut result = Vec::with_capacity(entities.len());
    for entity in entities {
        if skip_bot_commands && entity.r#type == Some(Type::BotCommand) {
            continue;
        }
        if entity.r#type == Some(Type::MediaTimestamp)
            && max_media_timestamp < entity.media_timestamp
        {
            continue;
        }
        // The C++ drops the entity when the type object is null; that is
        // only reachable through the UNREACHABLE Size case, which the port
        // panics on instead.
        let user_manager = user_manager.as_deref_mut();
        result.push(entity.get_text_entity_object(user_manager));
    }
    result
}

/// `get_formatted_text_object` (MessageEntity.cpp:210-215).
pub fn get_formatted_text_object<U: UserManagerLike + ?Sized>(
    user_manager: Option<&mut U>,
    text: &FormattedText,
    skip_bot_commands: bool,
    max_media_timestamp: i32,
) -> td_api::FormattedText {
    td_api::FormattedText {
        text: text.text.clone(),
        entities: get_text_entities_object(
            user_manager,
            &text.entities,
            skip_bot_commands,
            max_media_timestamp,
        ),
    }
}

/// `remove_empty_entities` (MessageEntity.cpp:1444-1466) — needed early:
/// `FormattedText::parse` applies it. Full fixups land with M.8.5.
pub fn remove_empty_entities(entities: &mut Vec<MessageEntity>) {
    entities.retain(|entity| {
        if entity.length <= 0 {
            return false;
        }
        match entity.r#type {
            Some(Type::TextUrl) => !entity.argument.is_empty(),
            Some(Type::MentionName) => entity.user_id.is_valid(),
            Some(Type::CustomEmoji) => entity.custom_emoji_id.is_valid(),
            Some(Type::FormattedDate) => entity.date.is_valid(),
            _ => true,
        }
    });
}

// ---------------------------------------------------------------------------
// Type masks (MessageEntity.cpp:1488-1560)
// ---------------------------------------------------------------------------

/// `get_entity_type_mask` (MessageEntity.cpp:1488-1490).
fn entity_type_mask(r#type: Type) -> i32 {
    1 << (r#type as i32)
}

/// `get_splittable_entities_mask` (MessageEntity.cpp:1492-1497).
fn splittable_entities_mask() -> i32 {
    use Type::*;
    entity_type_mask(Bold)
        | entity_type_mask(Italic)
        | entity_type_mask(Underline)
        | entity_type_mask(Strikethrough)
        | entity_type_mask(Spoiler)
}

/// `get_blockquote_entities_mask` (MessageEntity.cpp:1499-1503).
fn blockquote_entities_mask() -> i32 {
    entity_type_mask(Type::BlockQuote) | entity_type_mask(Type::ExpandableBlockQuote)
}

/// `get_continuous_entities_mask` (MessageEntity.cpp:1505-1516).
fn continuous_entities_mask() -> i32 {
    use Type::*;
    entity_type_mask(Mention)
        | entity_type_mask(Hashtag)
        | entity_type_mask(BotCommand)
        | entity_type_mask(Url)
        | entity_type_mask(EmailAddress)
        | entity_type_mask(TextUrl)
        | entity_type_mask(MentionName)
        | entity_type_mask(Cashtag)
        | entity_type_mask(PhoneNumber)
        | entity_type_mask(BankCardNumber)
        | entity_type_mask(MediaTimestamp)
        | entity_type_mask(CustomEmoji)
}

/// `get_pre_entities_mask` (MessageEntity.cpp:1518-1522).
fn pre_entities_mask() -> i32 {
    use Type::*;
    entity_type_mask(Pre)
        | entity_type_mask(Code)
        | entity_type_mask(PreCode)
        | entity_type_mask(FormattedDate)
}

/// `get_user_entities_mask` (MessageEntity.cpp:1524-1531).
fn user_entities_mask() -> i32 {
    splittable_entities_mask()
        | blockquote_entities_mask()
        | entity_type_mask(Type::TextUrl)
        | entity_type_mask(Type::MentionName)
        | entity_type_mask(Type::CustomEmoji)
        | pre_entities_mask()
}

/// `is_splittable_entity` (MessageEntity.cpp:1533-1535).
pub(crate) fn is_splittable_entity(r#type: Type) -> bool {
    (entity_type_mask(r#type) & splittable_entities_mask()) != 0
}

/// `is_blockquote_entity` (MessageEntity.cpp:1537-1539).
pub(crate) fn is_blockquote_entity(r#type: Type) -> bool {
    (entity_type_mask(r#type) & blockquote_entities_mask()) != 0
}

/// `is_continuous_entity` (MessageEntity.cpp:1541-1543).
pub(crate) fn is_continuous_entity(r#type: Type) -> bool {
    (entity_type_mask(r#type) & continuous_entities_mask()) != 0
}

/// `is_pre_entity` (MessageEntity.cpp:1545-1547).
pub(crate) fn is_pre_entity(r#type: Type) -> bool {
    (entity_type_mask(r#type) & pre_entities_mask()) != 0
}

/// `is_user_entity` (MessageEntity.cpp:1549-1551).
pub(crate) fn is_user_entity(r#type: Type) -> bool {
    (entity_type_mask(r#type) & user_entities_mask()) != 0
}

/// `SPLITTABLE_ENTITY_TYPE_COUNT` (MessageEntity.cpp:1553).
pub(crate) const SPLITTABLE_ENTITY_TYPE_COUNT: usize = 5;

/// `get_splittable_entity_type_index` (MessageEntity.cpp:1555-1566):
/// Bold, Italic, Underline, Strikethrough, Spoiler — in that order.
pub(crate) fn splittable_entity_type_index(r#type: Type) -> usize {
    use Type::*;
    let t = r#type as i32;
    if t <= Bold as i32 + 1 {
        // Bold or Italic
        (t - Bold as i32) as usize
    } else if t <= Underline as i32 + 1 {
        // Underline or Strikethrough
        (t - Underline as i32 + 2) as usize
    } else {
        assert_eq!(r#type, Spoiler, "the C++ CHECK(type == Type::Spoiler)");
        4
    }
}

// ---------------------------------------------------------------------------
// td_api input converter (MessageEntity.cpp:3669-3851)
// ---------------------------------------------------------------------------

/// `get_message_entities` over td_api input (MessageEntity.cpp:3669-3851).
/// The generated td_api cannot express a null `textEntity`, so only the
/// null-type skip differs structurally from the C++ loop.
pub fn get_message_entities<U: UserManagerLike + ?Sized>(
    mut user_manager: Option<&mut U>,
    input_entities: Vec<td_api::TextEntity>,
    allow_all: bool,
) -> Result<Vec<MessageEntity>, Status> {
    use crate::link_manager::{check_link, get_link_user_id};
    use crate::misc::clean_input_string;

    let mut entities: Vec<MessageEntity> = Vec::with_capacity(input_entities.len());
    for input_entity in input_entities {
        let offset = input_entity.offset;
        let length = input_entity.length;
        match input_entity.r#type.as_ref().unwrap() {
            td_api::TextEntityType::textEntityTypeMention(_) => {
                entities.push(MessageEntity::new(Type::Mention, offset, length));
            }
            td_api::TextEntityType::textEntityTypeHashtag(_) => {
                entities.push(MessageEntity::new(Type::Hashtag, offset, length));
            }
            td_api::TextEntityType::textEntityTypeBotCommand(_) => {
                entities.push(MessageEntity::new(Type::BotCommand, offset, length));
            }
            td_api::TextEntityType::textEntityTypeUrl(_) => {
                entities.push(MessageEntity::new(Type::Url, offset, length));
            }
            td_api::TextEntityType::textEntityTypeEmailAddress(_) => {
                entities.push(MessageEntity::new(Type::EmailAddress, offset, length));
            }
            td_api::TextEntityType::textEntityTypeCashtag(_) => {
                entities.push(MessageEntity::new(Type::Cashtag, offset, length));
            }
            td_api::TextEntityType::textEntityTypePhoneNumber(_) => {
                entities.push(MessageEntity::new(Type::PhoneNumber, offset, length));
            }
            td_api::TextEntityType::textEntityTypeBankCardNumber(_) => {
                entities.push(MessageEntity::new(Type::BankCardNumber, offset, length));
            }
            td_api::TextEntityType::textEntityTypeBold(_) => {
                entities.push(MessageEntity::new(Type::Bold, offset, length));
            }
            td_api::TextEntityType::textEntityTypeItalic(_) => {
                entities.push(MessageEntity::new(Type::Italic, offset, length));
            }
            td_api::TextEntityType::textEntityTypeUnderline(_) => {
                entities.push(MessageEntity::new(Type::Underline, offset, length));
            }
            td_api::TextEntityType::textEntityTypeStrikethrough(_) => {
                entities.push(MessageEntity::new(Type::Strikethrough, offset, length));
            }
            td_api::TextEntityType::textEntityTypeBlockQuote(_) => {
                entities.push(MessageEntity::new(Type::BlockQuote, offset, length));
            }
            td_api::TextEntityType::textEntityTypeCode(_) => {
                entities.push(MessageEntity::new(Type::Code, offset, length));
            }
            td_api::TextEntityType::textEntityTypePre(_) => {
                entities.push(MessageEntity::new(Type::Pre, offset, length));
            }
            td_api::TextEntityType::textEntityTypePreCode(entity) => {
                let Some(language) = clean_input_string(&entity.language) else {
                    return Err(Status::error(
                        400,
                        "MessageEntityPreCode.language must be encoded in UTF-8",
                    ));
                };
                entities.push(MessageEntity::with_argument(
                    Type::PreCode,
                    offset,
                    length,
                    language,
                ));
            }
            td_api::TextEntityType::textEntityTypeTextUrl(entity) => {
                let Some(url) = clean_input_string(&entity.url) else {
                    return Err(Status::error(
                        400,
                        "MessageEntityTextUrl.url must be encoded in UTF-8",
                    ));
                };
                let user_id = get_link_user_id(&url);
                if user_id.is_valid() {
                    if let Some(user_manager) = user_manager.as_mut() {
                        (*user_manager).ensure_input_user(user_id)?;
                    }
                    entities.push(MessageEntity::mention_name(offset, length, user_id));
                } else {
                    let r_url = check_link(&url, false, false).map_err(|error| {
                        Status::error(400, &format!("Entity {}", error.message()))
                    })?;
                    entities.push(MessageEntity::with_argument(
                        Type::TextUrl,
                        offset,
                        length,
                        r_url,
                    ));
                }
            }
            td_api::TextEntityType::textEntityTypeMentionName(entity) => {
                let user_id = UserId::new(entity.user_id);
                if let Some(user_manager) = user_manager.as_mut() {
                    (*user_manager).ensure_input_user(user_id)?;
                }
                entities.push(MessageEntity::mention_name(offset, length, user_id));
            }
            td_api::TextEntityType::textEntityTypeMediaTimestamp(entity) => {
                if entity.media_timestamp < 0 {
                    return Err(Status::error(400, "Invalid media timestamp specified"));
                }
                entities.push(MessageEntity::media_timestamp(
                    offset,
                    length,
                    entity.media_timestamp,
                ));
            }
            td_api::TextEntityType::textEntityTypeSpoiler(_) => {
                entities.push(MessageEntity::new(Type::Spoiler, offset, length));
            }
            td_api::TextEntityType::textEntityTypeCustomEmoji(entity) => {
                let custom_emoji_id = CustomEmojiId::new(entity.custom_emoji_id);
                if !custom_emoji_id.is_valid() {
                    return Err(Status::error(
                        400,
                        "Invalid custom emoji identifier specified",
                    ));
                }
                entities.push(MessageEntity::custom_emoji(offset, length, custom_emoji_id));
            }
            td_api::TextEntityType::textEntityTypeExpandableBlockQuote(_) => {
                entities.push(MessageEntity::new(
                    Type::ExpandableBlockQuote,
                    offset,
                    length,
                ));
            }
            td_api::TextEntityType::textEntityTypeDateTime(entity) => {
                let date = FormattedDate::get_formatted_date(
                    entity.unix_time,
                    entity.formatting_type.as_ref(),
                )?;
                entities.push(MessageEntity::formatted_date(offset, length, date));
            }
        }
        // CHECK(!entities.empty()) — trivially true after the push.
        if !allow_all {
            let Some(last) = entities.last() else {
                unreachable!("an entity was pushed in every branch above")
            };
            let Some(last_type) = last.r#type else {
                unreachable!("pushed entities always carry a type")
            };
            if !is_user_entity(last_type) {
                entities.pop();
            }
        }
    }
    Ok(entities)
}

// ---------------------------------------------------------------------------
// telegram_api input converter (MessageEntity.cpp:3804-3953)
// ---------------------------------------------------------------------------

/// `get_message_entities` over server `telegram_api` input
/// (MessageEntity.cpp:3804-3953).
pub fn get_server_message_entities<U: UserManagerLike + ?Sized>(
    mut user_manager: Option<&mut U>,
    server_entities: Vec<tdmtproto::telegram_api::MessageEntity>,
    source: &str,
) -> Vec<MessageEntity> {
    use tdmtproto::telegram_api;
    let mut entities: Vec<MessageEntity> = Vec::with_capacity(server_entities.len());
    for server_entity in server_entities {
        match server_entity {
            telegram_api::MessageEntity::messageEntityUnknown(_) => {}
            telegram_api::MessageEntity::messageEntityMention(entity) => {
                entities.push(MessageEntity::new(
                    Type::Mention,
                    entity.offset,
                    entity.length,
                ));
            }
            telegram_api::MessageEntity::messageEntityHashtag(entity) => {
                entities.push(MessageEntity::new(
                    Type::Hashtag,
                    entity.offset,
                    entity.length,
                ));
            }
            telegram_api::MessageEntity::messageEntityCashtag(entity) => {
                entities.push(MessageEntity::new(
                    Type::Cashtag,
                    entity.offset,
                    entity.length,
                ));
            }
            telegram_api::MessageEntity::messageEntityPhone(entity) => {
                entities.push(MessageEntity::new(
                    Type::PhoneNumber,
                    entity.offset,
                    entity.length,
                ));
            }
            telegram_api::MessageEntity::messageEntityBotCommand(entity) => {
                entities.push(MessageEntity::new(
                    Type::BotCommand,
                    entity.offset,
                    entity.length,
                ));
            }
            telegram_api::MessageEntity::messageEntityBankCard(entity) => {
                entities.push(MessageEntity::new(
                    Type::BankCardNumber,
                    entity.offset,
                    entity.length,
                ));
            }
            telegram_api::MessageEntity::messageEntityUrl(entity) => {
                entities.push(MessageEntity::new(Type::Url, entity.offset, entity.length));
            }
            telegram_api::MessageEntity::messageEntityEmail(entity) => {
                entities.push(MessageEntity::new(
                    Type::EmailAddress,
                    entity.offset,
                    entity.length,
                ));
            }
            telegram_api::MessageEntity::messageEntityBold(entity) => {
                entities.push(MessageEntity::new(Type::Bold, entity.offset, entity.length));
            }
            telegram_api::MessageEntity::messageEntityItalic(entity) => {
                entities.push(MessageEntity::new(
                    Type::Italic,
                    entity.offset,
                    entity.length,
                ));
            }
            telegram_api::MessageEntity::messageEntityUnderline(entity) => {
                entities.push(MessageEntity::new(
                    Type::Underline,
                    entity.offset,
                    entity.length,
                ));
            }
            telegram_api::MessageEntity::messageEntityStrike(entity) => {
                entities.push(MessageEntity::new(
                    Type::Strikethrough,
                    entity.offset,
                    entity.length,
                ));
            }
            telegram_api::MessageEntity::messageEntitySpoiler(entity) => {
                entities.push(MessageEntity::new(
                    Type::Spoiler,
                    entity.offset,
                    entity.length,
                ));
            }
            telegram_api::MessageEntity::messageEntityBlockquote(entity) => {
                let r#type = if entity.collapsed {
                    Type::ExpandableBlockQuote
                } else {
                    Type::BlockQuote
                };
                entities.push(MessageEntity::new(r#type, entity.offset, entity.length));
            }
            telegram_api::MessageEntity::messageEntityCode(entity) => {
                entities.push(MessageEntity::new(Type::Code, entity.offset, entity.length));
            }
            telegram_api::MessageEntity::messageEntityPre(entity) => {
                if entity.language.is_empty() {
                    entities.push(MessageEntity::new(Type::Pre, entity.offset, entity.length));
                } else {
                    entities.push(MessageEntity::with_argument(
                        Type::PreCode,
                        entity.offset,
                        entity.length,
                        entity.language,
                    ));
                }
            }
            telegram_api::MessageEntity::messageEntityTextUrl(entity) => {
                match crate::link_manager::check_link(&entity.url, false, false) {
                    Err(error) => {
                        tdutils::log_error!("Entity {} from {source}", error.message());
                        continue;
                    }
                    Ok(url) => {
                        entities.push(MessageEntity::with_argument(
                            Type::TextUrl,
                            entity.offset,
                            entity.length,
                            url,
                        ));
                    }
                }
            }
            telegram_api::MessageEntity::messageEntityMentionName(entity) => {
                let user_id = UserId::new(entity.user_id);
                if !user_id.is_valid() {
                    tdutils::log_error!("Receive invalid {user_id:?} in MentionName from {source}");
                    continue;
                }
                let Some(user_manager) = user_manager.as_mut() else {
                    tdutils::log_error!("Receive unknown {user_id:?} in MentionName from {source}");
                    continue;
                };
                if !user_manager.have_min_user(user_id) {
                    tdutils::log_error!("Receive wrong {user_id:?} from {source}");
                    continue;
                }
                entities.push(MessageEntity::mention_name(
                    entity.offset,
                    entity.length,
                    user_id,
                ));
            }
            telegram_api::MessageEntity::messageEntityCustomEmoji(entity) => {
                entities.push(MessageEntity::custom_emoji(
                    entity.offset,
                    entity.length,
                    CustomEmojiId::new(entity.document_id),
                ));
            }
            telegram_api::MessageEntity::messageEntityFormattedDate(entity) => {
                let date = FormattedDate::from_flags(
                    entity.date,
                    entity.relative,
                    entity.short_time,
                    entity.long_time,
                    entity.short_date,
                    entity.long_date,
                    entity.day_of_week,
                );
                if !date.is_valid() {
                    continue;
                }
                entities.push(MessageEntity::formatted_date(
                    entity.offset,
                    entity.length,
                    date,
                ));
            }
            telegram_api::MessageEntity::messageEntityDiffInsert(_)
            | telegram_api::MessageEntity::messageEntityDiffReplace(_)
            | telegram_api::MessageEntity::messageEntityDiffDelete(_) => {
                tdutils::log_error!("Receive {server_entity:?}");
                continue;
            }
            // The C++ `default: UNREACHABLE()` — includes
            // inputMessageEntityMentionName, which servers never send.
            _ => unreachable!("get_server_message_entities on an impossible entity"),
        }
    }
    entities
}

// ---------------------------------------------------------------------------
// secret_api input converter (MessageEntity.cpp:3955-4087)
// ---------------------------------------------------------------------------

/// The sticker-cache dependency of the secret converter and the quote
/// rules: `td->stickers_manager_->is_premium_custom_emoji(custom_emoji_id,
/// default_result)` (StickersManager.cpp:2065-2074). `None` models a cold
/// cache (unknown emoji → the C++ `default_result`), the state a real
/// cache starts in.
pub type IsPremiumCustomEmoji<'a> = Option<&'a mut dyn FnMut(CustomEmojiId, bool) -> bool>;

/// `get_message_entities` over `secret_api` input
/// (MessageEntity.cpp:3955-4087). The custom-emoji preload
/// (`get_custom_emoji_stickers` + the multipromise fan-out) lands with the
/// StickersManager step — the returned entities are identical, only the
/// cache warming is parked.
pub fn get_secret_message_entities(
    secret_entities: Vec<tdmtproto::secret_api::MessageEntity>,
    is_premium: bool,
    mut is_premium_custom_emoji: IsPremiumCustomEmoji<'_>,
) -> Vec<MessageEntity> {
    use tdmtproto::secret_api;

    const MAX_SECRET_CHAT_ENTITIES: usize = 1000;
    const MAX_CUSTOM_EMOJI_ENTITIES: usize = 100;

    let mut entities: Vec<MessageEntity> = Vec::with_capacity(secret_entities.len());
    let mut custom_emoji_ids: Vec<CustomEmojiId> = Vec::new();
    for secret_entity in secret_entities {
        match secret_entity {
            secret_api::MessageEntity::messageEntityUnknown(_) => {}
            // Mention/Hashtag/Cashtag/Phone/BotCommand/BankCard/Url/Email —
            // skipped, the finders re-discover them; bot commands and name
            // mentions are never restored in secret chats.
            secret_api::MessageEntity::messageEntityMention(_)
            | secret_api::MessageEntity::messageEntityHashtag(_)
            | secret_api::MessageEntity::messageEntityCashtag(_)
            | secret_api::MessageEntity::messageEntityPhone(_)
            | secret_api::MessageEntity::messageEntityBotCommand(_)
            | secret_api::MessageEntity::messageEntityBankCard(_)
            | secret_api::MessageEntity::messageEntityUrl(_)
            | secret_api::MessageEntity::messageEntityEmail(_)
            | secret_api::MessageEntity::messageEntityMentionName(_) => {}
            secret_api::MessageEntity::messageEntityBold(entity) => {
                entities.push(MessageEntity::new(Type::Bold, entity.offset, entity.length));
            }
            secret_api::MessageEntity::messageEntityItalic(entity) => {
                entities.push(MessageEntity::new(
                    Type::Italic,
                    entity.offset,
                    entity.length,
                ));
            }
            secret_api::MessageEntity::messageEntityUnderline(entity) => {
                entities.push(MessageEntity::new(
                    Type::Underline,
                    entity.offset,
                    entity.length,
                ));
            }
            secret_api::MessageEntity::messageEntityStrike(entity) => {
                entities.push(MessageEntity::new(
                    Type::Strikethrough,
                    entity.offset,
                    entity.length,
                ));
            }
            secret_api::MessageEntity::messageEntityBlockquote(entity) => {
                entities.push(MessageEntity::new(
                    Type::BlockQuote,
                    entity.offset,
                    entity.length,
                ));
            }
            secret_api::MessageEntity::messageEntityCode(entity) => {
                entities.push(MessageEntity::new(Type::Code, entity.offset, entity.length));
            }
            secret_api::MessageEntity::messageEntityPre(entity) => {
                // The generated secret_api carries strings as raw bytes;
                // the C++ `clean_input_string` gates them on UTF-8.
                let mut language = if tdutils::utf8::check_utf8(&entity.language) {
                    let text = String::from_utf8_lossy(&entity.language).into_owned();
                    crate::misc::clean_input_string(&text).unwrap_or_default()
                } else {
                    tdutils::log_warning!(
                        "Wrong language in entity: \"{}\"",
                        String::from_utf8_lossy(&entity.language)
                    );
                    String::new()
                };
                if language.is_empty() {
                    entities.push(MessageEntity::new(Type::Pre, entity.offset, entity.length));
                } else {
                    entities.push(MessageEntity::with_argument(
                        Type::PreCode,
                        entity.offset,
                        entity.length,
                        std::mem::take(&mut language),
                    ));
                }
            }
            secret_api::MessageEntity::messageEntityTextUrl(entity) => {
                if !tdutils::utf8::check_utf8(&entity.url) {
                    tdutils::log_warning!(
                        "Wrong URL entity: \"{}\"",
                        String::from_utf8_lossy(&entity.url)
                    );
                    continue;
                }
                let url = String::from_utf8_lossy(&entity.url).into_owned();
                match crate::link_manager::check_link(&url, false, false) {
                    Err(error) => {
                        tdutils::log_warning!("Entity {}", error.message());
                        continue;
                    }
                    Ok(url) => {
                        entities.push(MessageEntity::with_argument(
                            Type::TextUrl,
                            entity.offset,
                            entity.length,
                            url,
                        ));
                    }
                }
            }
            secret_api::MessageEntity::messageEntitySpoiler(entity) => {
                entities.push(MessageEntity::new(
                    Type::Spoiler,
                    entity.offset,
                    entity.length,
                ));
            }
            secret_api::MessageEntity::messageEntityCustomEmoji(entity) => {
                let custom_emoji_id = CustomEmojiId::new(entity.document_id);
                let is_premium_emoji = !is_premium
                    && match is_premium_custom_emoji.as_mut() {
                        Some(check) => check(custom_emoji_id, false),
                        None => false, // cold cache: unknown → the C++ default_result
                    };
                if !is_premium_emoji && custom_emoji_ids.len() < MAX_CUSTOM_EMOJI_ENTITIES {
                    entities.push(MessageEntity::custom_emoji(
                        entity.offset,
                        entity.length,
                        custom_emoji_id,
                    ));
                    custom_emoji_ids.push(custom_emoji_id);
                }
            }
        }

        if entities.len() >= MAX_SECRET_CHAT_ENTITIES {
            break;
        }
    }

    // The non-premium preload (`get_custom_emoji_stickers` with the
    // multipromise promise) is parked with StickersManager.

    entities
}

// ---------------------------------------------------------------------------
// Quote entity rules (MessageEntity.cpp:4840-4910)
// ---------------------------------------------------------------------------

/// `is_allowed_quote_entity_type` (MessageEntity.cpp:4840-4853).
pub fn is_allowed_quote_entity_type(r#type: Type) -> bool {
    matches!(
        r#type,
        Type::Bold
            | Type::Italic
            | Type::Underline
            | Type::Strikethrough
            | Type::Spoiler
            | Type::CustomEmoji
            | Type::FormattedDate
    )
}

/// `keep_only_custom_emoji` (MessageEntity.cpp:4855-4858): returns whether
/// anything was removed.
pub fn keep_only_custom_emoji(text: &mut FormattedText) -> bool {
    let before = text.entities.len();
    text.entities
        .retain(|entity| entity.r#type == Some(Type::CustomEmoji));
    before != text.entities.len()
}

/// `remove_unallowed_quote_entities` (MessageEntity.cpp:4901-4903).
pub fn remove_unallowed_quote_entities(text: &mut FormattedText) -> bool {
    let before = text.entities.len();
    text.entities
        .retain(|entity| entity.r#type.is_some_and(is_allowed_quote_entity_type));
    before != text.entities.len()
}

/// `remove_unallowed_quote_user_entities` (MessageEntity.cpp:4905-4910).
pub fn remove_unallowed_quote_user_entities(
    text: &mut FormattedText,
    skip_bot_commands: bool,
    skip_media_timestamps: bool,
) -> bool {
    let before = text.entities.len();
    text.entities.retain(|entity| {
        entity.r#type.is_some_and(|r#type| {
            is_allowed_quote_entity_type(r#type)
                || is_found_entity_type(r#type, skip_bot_commands, skip_media_timestamps)
        })
    });
    before != text.entities.len()
}

/// `remove_premium_custom_emoji_entities` (MessageEntity.cpp:4860-4865).
pub fn remove_premium_custom_emoji_entities(
    entities: &mut Vec<MessageEntity>,
    remove_unknown: bool,
    mut is_premium_custom_emoji: IsPremiumCustomEmoji<'_>,
) {
    entities.retain(|entity| {
        if entity.r#type != Some(Type::CustomEmoji) {
            return true;
        }
        match is_premium_custom_emoji.as_mut() {
            Some(check) => !check(entity.custom_emoji_id, remove_unknown),
            None => !remove_unknown, // cold cache: unknown emoji → the C++ default_result
        }
    });
}

/// `remove_unallowed_entities` (MessageEntity.cpp:4867-4899). The
/// Td-owned capabilities are threaded explicitly: the secret-chat layer
/// comes from the user manager, the premium check is the
/// `IsPremiumCustomEmoji` closure (`is_premium_custom_emoji(id, true)`),
/// and the dialog premium gate is the plain
/// `can_use_premium_custom_emoji_in_dialog` bool.
pub fn remove_unallowed_entities<U: UserManagerLike + ?Sized>(
    text: &mut FormattedText,
    dialog_id: DialogId,
    user_manager: Option<&U>,
    is_premium_custom_emoji: IsPremiumCustomEmoji<'_>,
    can_use_premium_custom_emoji_in_dialog: bool,
) {
    if text.entities.is_empty() {
        return;
    }

    if let DialogId::SecretChat(secret_chat_id) = dialog_id {
        let layer = user_manager.map_or(0, |user_manager| {
            user_manager.get_secret_chat_layer(secret_chat_id)
        });
        text.entities.retain(|entity| {
            let Some(r#type) = entity.r#type else {
                return false;
            };
            if layer < SECRET_CHAT_LAYER_NEW_ENTITIES
                && matches!(
                    r#type,
                    Type::Underline
                        | Type::Strikethrough
                        | Type::BlockQuote
                        | Type::ExpandableBlockQuote
                )
            {
                return false;
            }
            if layer < SECRET_CHAT_LAYER_SPOILER_AND_CUSTOM_EMOJI_ENTITIES
                && matches!(r#type, Type::Spoiler | Type::CustomEmoji)
            {
                return false;
            }
            r#type != Type::FormattedDate
        });

        if layer < SECRET_CHAT_LAYER_NEW_ENTITIES {
            sort_entities(&mut text.entities);
            remove_intersecting_entities(&mut text.entities);
        }
    }
    if !can_use_premium_custom_emoji_in_dialog {
        remove_premium_custom_emoji_entities(&mut text.entities, true, is_premium_custom_emoji);
    }
}

// ---------------------------------------------------------------------------
// Char classes + matchers (MessageEntity.cpp:237-1000, 1347-1443)
// ---------------------------------------------------------------------------

/// `is_word_character` (MessageEntity.cpp:237-248).
fn is_word_character(code: u32) -> bool {
    use tdutils::unicode::UnicodeSimpleCategory::{DecimalNumber, Letter, Number};
    matches!(
        tdutils::unicode::get_unicode_simple_category(code),
        Letter | DecimalNumber | Number
    ) || code == u32::from(b'_')
}

/// `is_alpha_digit` (MessageEntity.cpp:254-256).
fn is_alpha_digit(code: u32) -> bool {
    (b'0' as u32..=b'9' as u32).contains(&code)
        || (b'a' as u32..=b'z' as u32).contains(&code)
        || (b'A' as u32..=b'Z' as u32).contains(&code)
}

/// `is_alpha_digit_or_underscore` (MessageEntity.cpp:258-260).
fn is_alpha_digit_or_underscore(code: u32) -> bool {
    is_alpha_digit(code) || code == u32::from(b'_')
}

/// `is_alpha_digit_or_underscore_or_minus` (MessageEntity.cpp:262-264).
fn is_alpha_digit_or_underscore_or_minus(code: u32) -> bool {
    is_alpha_digit_or_underscore(code) || code == u32::from(b'-')
}

/// `prev_utf8_unsafe` (utf8.cpp): the start index of the code point before
/// `pos`. The caller guarantees a non-continuation byte exists before it.
fn prev_utf8_unsafe(bytes: &[u8], pos: usize) -> usize {
    let mut pos = pos;
    loop {
        pos -= 1;
        if bytes[pos] & 0xC0 != 0x80 {
            return pos;
        }
    }
}

/// The code point starting at `pos` (already validated UTF-8 by the input
/// being a `&str`).
fn utf8_at(bytes: &[u8], pos: usize) -> u32 {
    tdutils::utf8::next_utf8_unsafe(&bytes[pos..]).0
}

/// `match_mentions` (MessageEntity.cpp:268-311): the
/// `(?<=\B)@([a-zA-Z0-9_]{2,32})(?=\b)` scan.
fn match_mentions(s: &str) -> Vec<&str> {
    let mut result = Vec::new();
    let bytes = s.as_bytes();
    let begin = 0usize;
    let end = bytes.len();
    let mut ptr = begin;

    while let Some(found) = bytes[ptr..].iter().position(|&b| b == b'@') {
        ptr += found;

        if ptr != begin && is_word_character(utf8_at(bytes, prev_utf8_unsafe(bytes, ptr))) {
            ptr += 1;
            continue;
        }
        ptr += 1;
        let mention_begin = ptr;
        while ptr != end && is_alpha_digit_or_underscore(u32::from(bytes[ptr])) {
            ptr += 1;
        }
        let mention_end = ptr;
        let mention_size = mention_end - mention_begin;
        if !(2..=32).contains(&mention_size) {
            continue;
        }
        let next = if ptr != end { utf8_at(bytes, ptr) } else { 0 };
        if is_word_character(next) {
            continue;
        }
        result.push(&s[mention_begin - 1..mention_end]);
    }
    result
}

/// `match_bot_commands` (MessageEntity.cpp:312-384): the
/// `(?<!\b|[\/<>])\/([a-zA-Z0-9_]{1,64})(?:@([a-zA-Z0-9_]{3,32}))?(?!\B|[\/<>])`
/// scan.
fn match_bot_commands(s: &str) -> Vec<&str> {
    let mut result = Vec::new();
    let bytes = s.as_bytes();
    let begin = 0usize;
    let end = bytes.len();
    let mut ptr = begin;

    while let Some(found) = bytes[ptr..].iter().position(|&b| b == b'/') {
        ptr += found;

        if ptr != begin {
            let prev = utf8_at(bytes, prev_utf8_unsafe(bytes, ptr));
            if is_word_character(prev)
                || prev == u32::from(b'/')
                || prev == u32::from(b'<')
                || prev == u32::from(b'>')
            {
                ptr += 1;
                continue;
            }
        }

        ptr += 1;
        let command_begin = ptr;
        while ptr != end && is_alpha_digit_or_underscore(u32::from(bytes[ptr])) {
            ptr += 1;
        }
        let mut command_end = ptr;
        let command_size = command_end - command_begin;
        if !(1..=64).contains(&command_size) {
            continue;
        }

        if ptr != end && bytes[ptr] == b'@' {
            ptr += 1;
            let mention_begin = ptr;
            while ptr != end && is_alpha_digit_or_underscore(u32::from(bytes[ptr])) {
                ptr += 1;
            }
            let mention_size = ptr - mention_begin;
            if !(3..=32).contains(&mention_size) {
                continue;
            }
            command_end = ptr;
        }

        let next = if ptr != end { utf8_at(bytes, ptr) } else { 0 };
        if is_word_character(next)
            || next == u32::from(b'/')
            || next == u32::from(b'<')
            || next == u32::from(b'>')
        {
            continue;
        }
        result.push(&s[command_begin - 1..command_end]);
    }
    result
}

/// `is_hashtag_letter` (MessageEntity.cpp:385-399): letters, decimal
/// numbers, `_`, U+200C, U+00B7 and the 0xd80..=0xdff range — the category
/// is written back for the caller's letter check.
fn is_hashtag_letter(c: u32) -> (bool, tdutils::unicode::UnicodeSimpleCategory) {
    use tdutils::unicode::UnicodeSimpleCategory;
    let category = tdutils::unicode::get_unicode_simple_category(c);
    let is_hashtag = c == u32::from(b'_')
        || c == 0x200c
        || c == 0xb7
        || (0xd80..=0xdff).contains(&c)
        || matches!(
            category,
            UnicodeSimpleCategory::DecimalNumber | UnicodeSimpleCategory::Letter
        );
    (is_hashtag, category)
}

/// `match_hashtags` (MessageEntity.cpp:401-457): the
/// `(?<=^|[^is_hashtag_letter])#([is_hashtag_letter]{1,256})(?:@([a-zA-Z0-9_]{3,32}))?(?![is_hashtag_letter]*#)`
/// scan with the at-least-one-letter rule and the 255-letter cut.
fn match_hashtags(s: &str) -> Vec<&str> {
    let mut result = Vec::new();
    let bytes = s.as_bytes();
    let begin = 0usize;
    let end = bytes.len();
    let mut ptr = begin;

    while let Some(found) = bytes[ptr..].iter().position(|&b| b == b'#') {
        ptr += found;

        if ptr != begin && is_hashtag_letter(utf8_at(bytes, prev_utf8_unsafe(bytes, ptr))).0 {
            ptr += 1;
            continue;
        }
        ptr += 1;
        let hashtag_begin = ptr;
        let mut hashtag_size = 0usize;
        let mut hashtag_end: Option<usize> = None;
        let mut was_letter = false;
        while ptr != end {
            let (code, code_size) = tdutils::utf8::next_utf8_unsafe(&bytes[ptr..]);
            let (is_hashtag, category) = is_hashtag_letter(code);
            if !is_hashtag {
                break;
            }
            ptr += code_size;

            if hashtag_size == 255 {
                hashtag_end = Some(ptr);
            }
            if hashtag_size != 256 {
                was_letter |= category == tdutils::unicode::UnicodeSimpleCategory::Letter;
                hashtag_size += 1;
            }
        }
        let mut hashtag_end = hashtag_end.unwrap_or(ptr);
        if hashtag_size < 1 {
            continue;
        }
        if hashtag_end == ptr && ptr != end && bytes[ptr] == b'@' {
            let mut username_end = ptr + 1;
            while username_end != end
                && username_end - ptr < 33
                && is_alpha_digit_or_underscore(u32::from(bytes[username_end]))
            {
                username_end += 1;
            }
            let length = username_end - ptr - 1;
            if length >= 3 {
                ptr = username_end;
                hashtag_end = username_end;
            }
        }
        if ptr != end && bytes[ptr] == b'#' {
            continue;
        }
        if !was_letter {
            continue;
        }
        result.push(&s[hashtag_begin - 1..hashtag_end]);
    }
    result
}

/// `match_cashtags` (MessageEntity.cpp:459-520): `$` + "1INCH" or
/// `[A-Z]{1,8}`, an optional `@username`, and boundary checks.
fn match_cashtags(s: &str) -> Vec<&str> {
    let mut result = Vec::new();
    let bytes = s.as_bytes();
    let begin = 0usize;
    let end = bytes.len();
    let mut ptr = begin;

    while let Some(found) = bytes[ptr..].iter().position(|&b| b == b'$') {
        ptr += found;

        if ptr != begin {
            let prev = utf8_at(bytes, prev_utf8_unsafe(bytes, ptr));
            let (prev_is_hashtag_letter, _) = is_hashtag_letter(prev);
            if prev_is_hashtag_letter || prev == u32::from(b'$') {
                ptr += 1;
                continue;
            }
        }

        ptr += 1;
        let cashtag_begin = ptr;
        if end - ptr >= 5 && &bytes[ptr..ptr + 5] == b"1INCH" {
            ptr += 5;
        } else {
            while ptr != end && bytes[ptr].is_ascii_uppercase() {
                ptr += 1;
            }
        }
        let mut cashtag_end = ptr;
        let cashtag_size = cashtag_end - cashtag_begin;
        if !(1..=8).contains(&cashtag_size) {
            continue;
        }
        if ptr != end && bytes[ptr] == b'@' {
            let mut username_end = ptr + 1;
            while username_end != end
                && is_alpha_digit_or_underscore(u32::from(bytes[username_end]))
            {
                username_end += 1;
            }
            let length = username_end - ptr - 1;
            if (3..=32).contains(&length) {
                cashtag_end = username_end;
                ptr = username_end;
            }
        }
        if cashtag_end != end {
            let code = utf8_at(bytes, ptr);
            let (code_is_hashtag_letter, _) = is_hashtag_letter(code);
            if code_is_hashtag_letter || code == u32::from(b'$') {
                continue;
            }
        }

        result.push(&s[cashtag_begin - 1..cashtag_end]);
    }
    result
}

/// `match_media_timestamps` (MessageEntity.cpp:521-570).
fn match_media_timestamps(s: &str) -> Vec<&str> {
    let mut result = Vec::new();
    let bytes = s.as_bytes();
    let begin = 0usize;
    let end = bytes.len();
    let mut ptr = begin;

    while let Some(found) = bytes[ptr..].iter().position(|&b| b == b':') {
        ptr += found;
        let colon_pos = ptr;

        let mut media_timestamp_begin = ptr;
        while media_timestamp_begin != begin
            && (bytes[media_timestamp_begin - 1] == b':'
                || bytes[media_timestamp_begin - 1].is_ascii_digit())
        {
            media_timestamp_begin -= 1;
        }
        let mut media_timestamp_end = ptr;
        while media_timestamp_end + 1 != end
            && (bytes[media_timestamp_end + 1] == b':'
                || bytes[media_timestamp_end + 1].is_ascii_digit())
        {
            media_timestamp_end += 1;
        }
        media_timestamp_end += 1;

        if media_timestamp_begin != colon_pos
            && media_timestamp_end != colon_pos + 1
            // The C++ reads ptr[1] past the end (the NUL terminator, not a
            // digit); guard the trailing-':' case equivalently.
            && colon_pos + 1 != end
            && bytes[colon_pos + 1].is_ascii_digit()
        {
            ptr = media_timestamp_end;

            if media_timestamp_begin != begin
                && is_word_character(utf8_at(
                    bytes,
                    prev_utf8_unsafe(bytes, media_timestamp_begin),
                ))
            {
                continue;
            }
            if media_timestamp_end != end && is_word_character(utf8_at(bytes, media_timestamp_end))
            {
                continue;
            }

            result.push(&s[media_timestamp_begin..media_timestamp_end]);
        } else {
            ptr = media_timestamp_end;
        }
    }
    result
}

/// `match_bank_card_numbers` (MessageEntity.cpp:572-635): the
/// `(?<=^|[^+_\pL\d-.,])[\d -]{13,}([^_\pL\d-]|$)` scan.
fn match_bank_card_numbers(s: &str) -> Vec<&str> {
    let mut result = Vec::new();
    let bytes = s.as_bytes();
    let begin = 0usize;
    let end = bytes.len();
    let mut ptr = begin;

    loop {
        while ptr != end && !bytes[ptr].is_ascii_digit() {
            ptr += 1;
        }
        if ptr == end {
            break;
        }
        if ptr != begin {
            let prev = utf8_at(bytes, prev_utf8_unsafe(bytes, ptr));
            use tdutils::unicode::UnicodeSimpleCategory::Letter;
            if prev == u32::from(b'.')
                || prev == u32::from(b',')
                || prev == u32::from(b'+')
                || prev == u32::from(b'-')
                || prev == u32::from(b'_')
                || tdutils::unicode::get_unicode_simple_category(prev) == Letter
            {
                while ptr != end
                    && (bytes[ptr].is_ascii_digit() || bytes[ptr] == b' ' || bytes[ptr] == b'-')
                {
                    ptr += 1;
                }
                continue;
            }
        }

        let card_number_begin = ptr;
        let mut digit_count = 0usize;
        while ptr != end
            && (bytes[ptr].is_ascii_digit() || bytes[ptr] == b' ' || bytes[ptr] == b'-')
        {
            if bytes[ptr] == b' '
                && (16..=19).contains(&digit_count)
                && digit_count == ptr - card_number_begin
            {
                // continuous card number
                break;
            }
            digit_count += usize::from(bytes[ptr].is_ascii_digit());
            ptr += 1;
        }
        if !(13..=19).contains(&digit_count) {
            continue;
        }

        let mut card_number_end = ptr;
        while !bytes[card_number_end - 1].is_ascii_digit() {
            card_number_end -= 1;
        }
        let card_number_size = card_number_end - card_number_begin;
        if card_number_size > 2 * digit_count - 1 {
            continue;
        }
        if card_number_end != end {
            let next = utf8_at(bytes, card_number_end);
            use tdutils::unicode::UnicodeSimpleCategory::Letter;
            if next == u32::from(b'-')
                || next == u32::from(b'_')
                || tdutils::unicode::get_unicode_simple_category(next) == Letter
            {
                continue;
            }
        }

        result.push(&s[card_number_begin..card_number_end]);
    }
    result
}

/// `is_valid_bank_card` (MessageEntity.cpp:943-995): the Luhn sum plus the
/// network prefix/length table.
fn is_valid_bank_card(s: &str) -> bool {
    const MIN_CARD_LENGTH: usize = 13;
    const MAX_CARD_LENGTH: usize = 19;
    let bytes = s.as_bytes();
    let mut digits = [0u8; MAX_CARD_LENGTH];
    let mut digit_count = 0usize;
    for &c in bytes {
        if c.is_ascii_digit() {
            assert!(
                digit_count < MAX_CARD_LENGTH,
                "the C++ CHECK(digit_count < MAX_CARD_LENGTH)"
            );
            digits[digit_count] = c;
            digit_count += 1;
        }
    }
    assert!(
        digit_count >= MIN_CARD_LENGTH,
        "the C++ CHECK(digit_count >= MIN_CARD_LENGTH)"
    );

    // Luhn algorithm
    let mut sum = 0i32;
    for i in (1..=digit_count).rev() {
        let digit = i32::from(digits[i - 1] - b'0');
        if (digit_count - i).is_multiple_of(2) {
            sum += digit;
        } else {
            sum += if digit < 5 { 2 * digit } else { 2 * digit - 9 };
        }
    }
    if sum % 10 != 0 {
        return false;
    }

    let prefix1 = i32::from(digits[0] - b'0');
    let prefix2 = prefix1 * 10 + i32::from(digits[1] - b'0');
    let prefix3 = prefix2 * 10 + i32::from(digits[2] - b'0');
    let prefix4 = prefix3 * 10 + i32::from(digits[3] - b'0');
    if prefix1 == 4 {
        // Visa
        return matches!(digit_count, 13 | 16 | 18 | 19);
    }
    if (51..=55).contains(&prefix2) || (2221..=2720).contains(&prefix4) {
        // mastercard
        return digit_count == 16;
    }
    if prefix2 == 34 || prefix2 == 37 {
        // American Express
        return digit_count == 15;
    }
    if prefix2 == 62 || prefix2 == 81 {
        // UnionPay
        return digit_count >= 16;
    }
    if (2200..=2204).contains(&prefix4) {
        // MIR
        return digit_count == 16;
    }
    true // skip length check
}

/// `is_email_address` (MessageEntity.cpp:997-1059).
pub fn is_email_address(s: &str) -> bool {
    // /^([a-z0-9_-]{0,26}[.+]){0,10}[a-z0-9_-]{1,35}@(([a-z0-9][a-z0-9_-]{0,28})?[a-z0-9][.]){1,6}[a-z]{2,8}$/i
    let (userdata, domain) = split_at(s, '@');
    if domain.is_empty() {
        return false;
    }

    let mut prev = 0usize;
    let mut userdata_part_count = 0usize;
    for (i, c) in userdata.bytes().enumerate() {
        if c == b'.' || c == b'+' {
            if i - prev >= 27 {
                return false;
            }
            userdata_part_count += 1;
            prev = i + 1;
        } else if !is_alpha_digit_or_underscore_or_minus(u32::from(c)) {
            return false;
        }
    }
    userdata_part_count += 1;
    if userdata_part_count >= 12 {
        return false;
    }
    let last_part_length = userdata.len() - prev;
    if last_part_length == 0 || last_part_length >= 36 {
        return false;
    }

    let domain_parts: Vec<&str> = domain.split('.').collect();
    if domain_parts.len() <= 1 || domain_parts.len() > 7 {
        return false;
    }
    let tld = domain_parts[domain_parts.len() - 1];
    if tld.len() <= 1 || tld.len() >= 9 {
        return false;
    }
    for c in tld.bytes() {
        if !c.is_ascii_alphabetic() {
            return false;
        }
    }
    let domain_parts = &domain_parts[..domain_parts.len() - 1];
    for part in domain_parts {
        if part.is_empty() || part.len() >= 31 {
            return false;
        }
        for c in part.bytes() {
            if !is_alpha_digit_or_underscore_or_minus(u32::from(c)) {
                return false;
            }
        }
        if !is_alpha_digit(u32::from(part.as_bytes()[0])) {
            return false;
        }
        if !is_alpha_digit(u32::from(part.as_bytes()[part.len() - 1])) {
            return false;
        }
    }

    true
}

/// `td::split(Slice, char)`: the first separator splits; the right side
/// keeps the rest (empty when the separator is missing).
fn split_at(s: &str, separator: char) -> (&str, &str) {
    match s.find(separator) {
        Some(pos) => (&s[..pos], &s[pos + separator.len_utf8()..]),
        None => (s, ""),
    }
}

/// `get_valid_short_usernames` (MessageEntity.cpp:1347-1350).
fn is_valid_short_username(mention: &str) -> bool {
    matches!(mention, "gif" | "nft" | "pic" | "ufc" | "vid")
}

/// `find_mentions` (MessageEntity.cpp:1352-1363): mentions survive when
/// they are at least four characters or a valid short username.
pub fn find_mentions(s: &str) -> Vec<&str> {
    match_mentions(s)
        .into_iter()
        .filter(|mention| {
            let mention = &mention[1..];
            if mention.len() >= 4 {
                true
            } else {
                is_valid_short_username(&mention.to_ascii_lowercase())
            }
        })
        .collect()
}

/// `find_bot_commands` (MessageEntity.cpp:1365-1367).
pub fn find_bot_commands(s: &str) -> Vec<&str> {
    match_bot_commands(s)
}

/// `find_hashtags` (MessageEntity.cpp:1369-1371).
pub fn find_hashtags(s: &str) -> Vec<&str> {
    match_hashtags(s)
}

/// `find_cashtags` (MessageEntity.cpp:1373-1375).
pub fn find_cashtags(s: &str) -> Vec<&str> {
    match_cashtags(s)
}

/// `find_bank_card_numbers` (MessageEntity.cpp:1377-1385): only the
/// Luhn-valid cards.
pub fn find_bank_card_numbers(s: &str) -> Vec<&str> {
    match_bank_card_numbers(s)
        .into_iter()
        .filter(|bank_card| is_valid_bank_card(bank_card))
        .collect()
}

/// `find_media_timestamps` (MessageEntity.cpp:1408-1442): the slice plus
/// the total number of seconds.
pub fn find_media_timestamps(s: &str) -> Vec<(&str, i32)> {
    let mut result = Vec::new();
    for media_timestamp in match_media_timestamps(s) {
        let parts: Vec<&str> = media_timestamp.split(':').collect();
        assert!(parts.len() >= 2, "the C++ CHECK(parts.size() >= 2)");
        if parts.len() > 3 || parts[parts.len() - 1].len() != 2 {
            continue;
        }
        let seconds: i32 = parts[parts.len() - 1].parse().unwrap();
        if seconds >= 60 {
            continue;
        }
        if parts.len() == 2 {
            if parts[0].len() > 4 || parts[0].is_empty() {
                continue;
            }

            let minutes: i32 = parts[0].parse().unwrap();
            result.push((media_timestamp, minutes * 60 + seconds));
        } else {
            if parts[0].len() > 2
                || parts[1].len() > 2
                || parts[0].is_empty()
                || parts[1].is_empty()
            {
                continue;
            }

            let minutes: i32 = parts[1].parse().unwrap();
            if minutes >= 60 {
                continue;
            }
            let hours: i32 = parts[0].parse().unwrap();
            result.push((media_timestamp, hours * 3600 + minutes * 60 + seconds));
        }
    }
    result
}

// ===== M.8.4 — URL machinery (MessageEntity.cpp:636-1443, 1830-1929) =====

/// `is_url_unicode_symbol` (MessageEntity.cpp:636-643).
fn is_url_unicode_symbol(code: u32) -> bool {
    if (0x2000..=0x206f).contains(&code) {
        // General Punctuation
        // Zero Width Non-Joiner/Joiner and various dashes
        return code == 0x200c || code == 0x200d || (0x2010..=0x2015).contains(&code);
    }
    tdutils::unicode::get_unicode_simple_category(code)
        != tdutils::unicode::UnicodeSimpleCategory::Separator
}

/// `is_url_path_symbol` (MessageEntity.cpp:645-656).
fn is_url_path_symbol(code: u32) -> bool {
    match code {
        0x0a => false, // '\n'
        0x3c => false, // '<'
        0x3e => false, // '>'
        0x22 => false, // '"'
        0xab => false, // «
        0xbb => false, // »
        _ => is_url_unicode_symbol(code),
    }
}

const BAD_PATH_END_CHARS: &[u8] = b".:;,('?!`";

/// `match_tg_urls` (MessageEntity.cpp:658-721): the
/// `(tg|ton|tonsite)://[a-z0-9_-]{1,253}([/?#][^\s\x{2000}-\x{200b}\x{200e}-\x{200f}\x{2016}-\x{206f}<>«»"]*)?`
/// scan.
fn match_tg_urls(s: &str) -> Vec<&str> {
    let mut result = Vec::new();
    let bytes = s.as_bytes();
    let begin = 0usize;
    let end = bytes.len();
    let mut ptr = begin;

    while end > ptr + 5 {
        let Some(found) = bytes[ptr..].iter().position(|&b| b == b':') else {
            break;
        };
        ptr += found;

        let mut url_begin = None;
        if end >= ptr + 3 && bytes[ptr + 1] == b'/' && bytes[ptr + 2] == b'/' {
            if ptr >= 2
                && bytes[ptr - 2].eq_ignore_ascii_case(&b't')
                && bytes[ptr - 1].eq_ignore_ascii_case(&b'g')
            {
                url_begin = Some(ptr - 2);
            } else if ptr >= 3
                && bytes[ptr - 3].eq_ignore_ascii_case(&b't')
                && bytes[ptr - 2].eq_ignore_ascii_case(&b'o')
                && bytes[ptr - 1].eq_ignore_ascii_case(&b'n')
            {
                url_begin = Some(ptr - 3);
            } else if ptr >= 7
                && bytes[ptr - 7].eq_ignore_ascii_case(&b't')
                && bytes[ptr - 6].eq_ignore_ascii_case(&b'o')
                && bytes[ptr - 5].eq_ignore_ascii_case(&b'n')
                && bytes[ptr - 4].eq_ignore_ascii_case(&b's')
                && bytes[ptr - 3].eq_ignore_ascii_case(&b'i')
                && bytes[ptr - 2].eq_ignore_ascii_case(&b't')
                && bytes[ptr - 1].eq_ignore_ascii_case(&b'e')
            {
                // Upstream quirk preserved verbatim: the matched slice begins
                // at `ptr - 3` ("ite://..."), not at the "tonsite" start.
                url_begin = Some(ptr - 3);
            }
        }
        let Some(url_begin) = url_begin else {
            ptr += 1;
            continue;
        };

        ptr += 3;
        let domain_begin = ptr;
        while ptr != end
            && ptr - domain_begin != 253
            && is_alpha_digit_or_underscore_or_minus(u32::from(bytes[ptr]))
        {
            ptr += 1;
        }
        if ptr == domain_begin {
            continue;
        }

        if ptr != end && matches!(bytes[ptr], b'/' | b'?' | b'#') {
            let mut path_end_ptr = ptr + 1;
            while path_end_ptr != end {
                let (code, width) = tdutils::utf8::next_utf8_unsafe(&bytes[path_end_ptr..]);
                if !is_url_path_symbol(code) {
                    break;
                }
                path_end_ptr += width;
            }
            while path_end_ptr > ptr + 1 && BAD_PATH_END_CHARS.contains(&bytes[path_end_ptr - 1]) {
                path_end_ptr -= 1;
            }
            if bytes[ptr] == b'/' || path_end_ptr > ptr + 1 {
                ptr = path_end_ptr;
            }
        }

        result.push(&s[url_begin..ptr]);
    }
    result
}

/// `match_urls` (MessageEntity.cpp:723-941).
fn match_urls(s: &str) -> Vec<&str> {
    let mut result = Vec::new();
    let bytes = s.as_bytes();
    let end = bytes.len();
    // `str` in the C++ loop; `begin` is its offset in `s`.
    let mut begin = 0usize;

    let is_protocol_symbol = |code: u32| -> bool {
        if code < 0x80 {
            // do not allow dots in the protocol
            return is_alpha_digit(code) || code == u32::from(b'+') || code == u32::from(b'-');
        }
        // add unicode letters and digits to later discard protocol as invalid
        tdutils::unicode::get_unicode_simple_category(code)
            != tdutils::unicode::UnicodeSimpleCategory::Separator
    };

    let is_user_data_symbol = |code: u32| -> bool {
        match code {
            0x0a => false, // '\n'
            0x2f => false, // '/'
            0x5b => false, // '['
            0x5d => false, // ']'
            0x7b => false, // '{'
            0x7d => false, // '}'
            0x28 => false, // '('
            0x29 => false, // ')'
            0x27 => false, // '\''
            0x60 => false, // '`'
            0x3c => false, // '<'
            0x3e => false, // '>'
            0x22 => false, // '"'
            0x40 => false, // '@'
            0xab => false, // «
            0xbb => false, // »
            _ => is_url_unicode_symbol(code),
        }
    };

    let is_domain_symbol = |code: u32| -> bool {
        if code < 0xc0 {
            return code == u32::from(b'.')
                || is_alpha_digit_or_underscore_or_minus(code)
                || code == u32::from(b'~');
        }
        is_url_unicode_symbol(code)
    };

    loop {
        let str_now = &s[begin..];
        let Some(dot_pos) = str_now.find('.') else {
            break;
        };
        if dot_pos + 1 == str_now.len() {
            break;
        }
        if str_now.as_bytes()[dot_pos + 1] == b' ' {
            // fast path
            begin += dot_pos + 2;
            continue;
        }

        let mut domain_begin_ptr = begin + dot_pos;
        while domain_begin_ptr != begin {
            domain_begin_ptr = prev_utf8_unsafe(bytes, domain_begin_ptr);
            let (code, width) = tdutils::utf8::next_utf8_unsafe(&bytes[domain_begin_ptr..]);
            if !is_domain_symbol(code) {
                domain_begin_ptr += width;
                break;
            }
        }

        let mut last_at_ptr = None;
        let mut domain_end_ptr = begin + dot_pos;
        while domain_end_ptr != end {
            let (code, width) = tdutils::utf8::next_utf8_unsafe(&bytes[domain_end_ptr..]);
            if code == u32::from(b'@') {
                last_at_ptr = Some(domain_end_ptr);
            } else if !is_domain_symbol(code) {
                break;
            }
            domain_end_ptr += width;
        }

        if last_at_ptr.is_some() {
            while domain_begin_ptr != begin {
                domain_begin_ptr = prev_utf8_unsafe(bytes, domain_begin_ptr);
                let (code, width) = tdutils::utf8::next_utf8_unsafe(&bytes[domain_begin_ptr..]);
                if !is_user_data_symbol(code) {
                    domain_begin_ptr += width;
                    break;
                }
            }
        }

        let mut url_end_ptr = domain_end_ptr;
        if url_end_ptr != end && bytes[url_end_ptr] == b':' {
            let mut port_end_ptr = url_end_ptr + 1;

            while port_end_ptr != end && bytes[port_end_ptr].is_ascii_digit() {
                port_end_ptr += 1;
            }

            let mut port_begin_ptr = url_end_ptr + 1;
            while port_begin_ptr != port_end_ptr && bytes[port_begin_ptr] == b'0' {
                port_begin_ptr += 1;
            }
            if port_begin_ptr != port_end_ptr
                && port_end_ptr - port_begin_ptr <= 5
                && std::str::from_utf8(&bytes[port_begin_ptr..port_end_ptr])
                    .is_ok_and(|port| port.parse::<u32>().is_ok_and(|port| port <= 65535))
            {
                url_end_ptr = port_end_ptr;
            }
        }

        if url_end_ptr != end && matches!(bytes[url_end_ptr], b'/' | b'?' | b'#') {
            let mut path_end_ptr = url_end_ptr + 1;
            while path_end_ptr != end {
                let (code, width) = tdutils::utf8::next_utf8_unsafe(&bytes[path_end_ptr..]);
                if !is_url_path_symbol(code) {
                    break;
                }
                path_end_ptr += width;
            }
            while path_end_ptr > url_end_ptr + 1
                && BAD_PATH_END_CHARS.contains(&bytes[path_end_ptr - 1])
            {
                path_end_ptr -= 1;
            }
            if bytes[url_end_ptr] == b'/' || path_end_ptr > url_end_ptr + 1 {
                url_end_ptr = path_end_ptr;
            }
        }
        while url_end_ptr > begin + dot_pos + 1 && bytes[url_end_ptr - 1] == b'.' {
            url_end_ptr -= 1;
        }

        let mut is_bad = false;
        let mut url_begin_ptr = domain_begin_ptr;
        if url_begin_ptr != begin && bytes[url_begin_ptr - 1] == b'@' {
            if last_at_ptr.is_some() {
                is_bad = true;
            }
            let mut user_data_begin_ptr = url_begin_ptr - 1;
            while user_data_begin_ptr != begin {
                user_data_begin_ptr = prev_utf8_unsafe(bytes, user_data_begin_ptr);
                let (code, width) = tdutils::utf8::next_utf8_unsafe(&bytes[user_data_begin_ptr..]);
                if !is_user_data_symbol(code) {
                    user_data_begin_ptr += width;
                    break;
                }
            }
            if user_data_begin_ptr == url_begin_ptr - 1 {
                is_bad = true;
            }
            url_begin_ptr = user_data_begin_ptr;
        }

        if url_begin_ptr != begin {
            let prefix = &s[begin..url_begin_ptr];
            if prefix.len() >= 6 && prefix.ends_with("://") {
                let mut protocol_begin_ptr = url_begin_ptr - 3;
                while protocol_begin_ptr != begin {
                    protocol_begin_ptr = prev_utf8_unsafe(bytes, protocol_begin_ptr);
                    let (code, width) =
                        tdutils::utf8::next_utf8_unsafe(&bytes[protocol_begin_ptr..]);
                    if !is_protocol_symbol(code) {
                        protocol_begin_ptr += width;
                        break;
                    }
                }
                let protocol = s[protocol_begin_ptr..url_begin_ptr - 3].to_ascii_lowercase();
                if protocol.ends_with("http") && protocol != "shttp" {
                    url_begin_ptr -= 7;
                } else if protocol.ends_with("https") {
                    url_begin_ptr -= 8;
                } else if protocol.ends_with("ftp") && protocol != "tftp" && protocol != "sftp" {
                    url_begin_ptr -= 6;
                } else if protocol.ends_with("tonsite") {
                    url_begin_ptr -= 10;
                } else {
                    is_bad = true;
                }
            } else {
                let prefix_back = prev_utf8_unsafe(bytes, url_begin_ptr);
                let (code, _) = tdutils::utf8::next_utf8_unsafe(&bytes[prefix_back..]);
                if is_word_character(code)
                    || code == u32::from(b'/')
                    || code == u32::from(b'#')
                    || code == u32::from(b'@')
                {
                    is_bad = true;
                }
            }
        }

        if !is_bad {
            if url_end_ptr > begin + dot_pos + 1 {
                result.push(&s[url_begin_ptr..url_end_ptr]);
            }
            while url_end_ptr != end && bytes[url_end_ptr] == b'.' {
                url_end_ptr += 1;
            }
        } else {
            while bytes[url_end_ptr - 1] != b'.' {
                url_end_ptr -= 1;
            }
        }

        if url_end_ptr <= begin + dot_pos {
            url_end_ptr = begin + dot_pos + 1;
        }
        begin = url_end_ptr;
    }

    result
}

/// The `is_common_tld` table (MessageEntity.cpp:1062-1178), sorted; only
/// membership is observable.
static COMMON_TLDS: &[&str] = &[
    "aaa",
    "aarp",
    "abb",
    "abbott",
    "abbvie",
    "abc",
    "able",
    "abogado",
    "abudhabi",
    "ac",
    "academy",
    "accenture",
    "accountant",
    "accountants",
    "aco",
    "actor",
    "ad",
    "ads",
    "adult",
    "ae",
    "aeg",
    "aero",
    "aetna",
    "af",
    "afl",
    "africa",
    "ag",
    "agakhan",
    "agency",
    "ai",
    "aig",
    "airbus",
    "airforce",
    "airtel",
    "akdn",
    "al",
    "alibaba",
    "alipay",
    "allfinanz",
    "allstate",
    "ally",
    "alsace",
    "alstom",
    "am",
    "amazon",
    "americanexpress",
    "americanfamily",
    "amex",
    "amfam",
    "amica",
    "amsterdam",
    "analytics",
    "android",
    "anquan",
    "anz",
    "ao",
    "aol",
    "apartments",
    "app",
    "apple",
    "aq",
    "aquarelle",
    "ar",
    "arab",
    "aramco",
    "archi",
    "army",
    "arpa",
    "art",
    "arte",
    "as",
    "asda",
    "asia",
    "associates",
    "at",
    "athleta",
    "attorney",
    "au",
    "auction",
    "audi",
    "audible",
    "audio",
    "auspost",
    "author",
    "auto",
    "autos",
    "aw",
    "aws",
    "ax",
    "axa",
    "az",
    "azure",
    "ba",
    "baby",
    "baidu",
    "banamex",
    "band",
    "bank",
    "bar",
    "barcelona",
    "barclaycard",
    "barclays",
    "barefoot",
    "bargains",
    "baseball",
    "basketball",
    "bauhaus",
    "bayern",
    "bb",
    "bbc",
    "bbt",
    "bbva",
    "bcg",
    "bcn",
    "bd",
    "be",
    "beats",
    "beauty",
    "beer",
    "bentley",
    "berlin",
    "best",
    "bestbuy",
    "bet",
    "bf",
    "bg",
    "bh",
    "bharti",
    "bi",
    "bible",
    "bid",
    "bike",
    "bing",
    "bingo",
    "bio",
    "biz",
    "bj",
    "black",
    "blackfriday",
    "blockbuster",
    "blog",
    "bloomberg",
    "blue",
    "bm",
    "bms",
    "bmw",
    "bn",
    "bnpparibas",
    "bo",
    "boats",
    "boehringer",
    "bofa",
    "bom",
    "bond",
    "boo",
    "book",
    "booking",
    "bosch",
    "bostik",
    "boston",
    "bot",
    "boutique",
    "box",
    "br",
    "bradesco",
    "bridgestone",
    "broadway",
    "broker",
    "brother",
    "brussels",
    "bs",
    "bt",
    "build",
    "builders",
    "business",
    "buy",
    "buzz",
    "bv",
    "bw",
    "by",
    "bz",
    "bzh",
    "ca",
    "cab",
    "cafe",
    "cal",
    "call",
    "calvinklein",
    "cam",
    "camera",
    "camp",
    "canon",
    "capetown",
    "capital",
    "capitalone",
    "car",
    "caravan",
    "cards",
    "care",
    "career",
    "careers",
    "cars",
    "casa",
    "case",
    "cash",
    "casino",
    "cat",
    "catering",
    "catholic",
    "cba",
    "cbn",
    "cbre",
    "cc",
    "cd",
    "center",
    "ceo",
    "cern",
    "cf",
    "cfa",
    "cfd",
    "cg",
    "ch",
    "chanel",
    "channel",
    "charity",
    "chase",
    "chat",
    "cheap",
    "chintai",
    "christmas",
    "chrome",
    "church",
    "ci",
    "cipriani",
    "circle",
    "cisco",
    "citadel",
    "citi",
    "citic",
    "city",
    "ck",
    "cl",
    "claims",
    "cleaning",
    "click",
    "clinic",
    "clinique",
    "clothing",
    "cloud",
    "club",
    "clubmed",
    "cm",
    "cn",
    "co",
    "coach",
    "codes",
    "coffee",
    "college",
    "cologne",
    "com",
    "commbank",
    "community",
    "company",
    "compare",
    "computer",
    "comsec",
    "condos",
    "construction",
    "consulting",
    "contact",
    "contractors",
    "cooking",
    "cool",
    "coop",
    "corsica",
    "country",
    "coupon",
    "coupons",
    "courses",
    "cpa",
    "cr",
    "credit",
    "creditcard",
    "creditunion",
    "cricket",
    "crown",
    "crs",
    "cruise",
    "cruises",
    "cu",
    "cuisinella",
    "cv",
    "cw",
    "cx",
    "cy",
    "cymru",
    "cyou",
    "cz",
    "dabur",
    "dad",
    "dance",
    "data",
    "date",
    "dating",
    "datsun",
    "day",
    "dclk",
    "dds",
    "de",
    "deal",
    "dealer",
    "deals",
    "degree",
    "delivery",
    "dell",
    "deloitte",
    "delta",
    "democrat",
    "dental",
    "dentist",
    "desi",
    "design",
    "dev",
    "dhl",
    "diamonds",
    "diet",
    "digital",
    "direct",
    "directory",
    "discount",
    "discover",
    "dish",
    "diy",
    "dj",
    "dk",
    "dm",
    "dnp",
    "do",
    "docs",
    "doctor",
    "dog",
    "domains",
    "dot",
    "download",
    "drive",
    "dtv",
    "dubai",
    "dunlop",
    "dupont",
    "durban",
    "dvag",
    "dvr",
    "dz",
    "earth",
    "eat",
    "ec",
    "eco",
    "edeka",
    "edu",
    "education",
    "ee",
    "eg",
    "email",
    "emerck",
    "energy",
    "engineer",
    "engineering",
    "enterprises",
    "epson",
    "equipment",
    "er",
    "ericsson",
    "erni",
    "es",
    "esq",
    "estate",
    "et",
    "eu",
    "eurovision",
    "eus",
    "events",
    "exchange",
    "expert",
    "exposed",
    "express",
    "extraspace",
    "fage",
    "fail",
    "fairwinds",
    "faith",
    "family",
    "fan",
    "fans",
    "farm",
    "farmers",
    "fashion",
    "fast",
    "fedex",
    "feedback",
    "ferrari",
    "ferrero",
    "fi",
    "fidelity",
    "fido",
    "film",
    "final",
    "finance",
    "financial",
    "fire",
    "firestone",
    "firmdale",
    "fish",
    "fishing",
    "fit",
    "fitness",
    "fj",
    "fk",
    "flickr",
    "flights",
    "flir",
    "florist",
    "flowers",
    "fly",
    "fm",
    "fo",
    "foo",
    "food",
    "football",
    "ford",
    "forex",
    "forsale",
    "forum",
    "foundation",
    "fox",
    "fr",
    "free",
    "fresenius",
    "frl",
    "frogans",
    "frontier",
    "ftr",
    "fujitsu",
    "fun",
    "fund",
    "furniture",
    "futbol",
    "fyi",
    "ga",
    "gal",
    "gallery",
    "gallo",
    "gallup",
    "game",
    "games",
    "gap",
    "garden",
    "gay",
    "gb",
    "gbiz",
    "gd",
    "gdn",
    "ge",
    "gea",
    "gent",
    "genting",
    "george",
    "gf",
    "gg",
    "ggee",
    "gh",
    "gi",
    "gift",
    "gifts",
    "gives",
    "giving",
    "gl",
    "glass",
    "gle",
    "global",
    "globo",
    "gm",
    "gmail",
    "gmbh",
    "gmo",
    "gmx",
    "gn",
    "godaddy",
    "gold",
    "goldpoint",
    "golf",
    "goo",
    "goodyear",
    "goog",
    "google",
    "gop",
    "got",
    "gov",
    "gp",
    "gq",
    "gr",
    "grainger",
    "graphics",
    "gratis",
    "green",
    "gripe",
    "grocery",
    "group",
    "gs",
    "gt",
    "gu",
    "gucci",
    "guge",
    "guide",
    "guitars",
    "guru",
    "gw",
    "gy",
    "hair",
    "hamburg",
    "hangout",
    "haus",
    "hbo",
    "hdfc",
    "hdfcbank",
    "health",
    "healthcare",
    "help",
    "helsinki",
    "here",
    "hermes",
    "hiphop",
    "hisamitsu",
    "hitachi",
    "hiv",
    "hk",
    "hkt",
    "hm",
    "hn",
    "hockey",
    "holdings",
    "holiday",
    "homedepot",
    "homegoods",
    "homes",
    "homesense",
    "honda",
    "horse",
    "hospital",
    "host",
    "hosting",
    "hot",
    "hotels",
    "hotmail",
    "house",
    "how",
    "hr",
    "hsbc",
    "ht",
    "hu",
    "hughes",
    "hyatt",
    "hyundai",
    "ibm",
    "icbc",
    "ice",
    "icu",
    "id",
    "ie",
    "ieee",
    "ifm",
    "ikano",
    "il",
    "im",
    "imamat",
    "imdb",
    "immo",
    "immobilien",
    "in",
    "inc",
    "industries",
    "infiniti",
    "info",
    "ing",
    "ink",
    "institute",
    "insurance",
    "insure",
    "int",
    "international",
    "intuit",
    "investments",
    "io",
    "ipiranga",
    "iq",
    "ir",
    "irish",
    "is",
    "ismaili",
    "ist",
    "istanbul",
    "it",
    "itau",
    "itv",
    "jaguar",
    "java",
    "jcb",
    "je",
    "jeep",
    "jetzt",
    "jewelry",
    "jio",
    "jll",
    "jm",
    "jmp",
    "jnj",
    "jo",
    "jobs",
    "joburg",
    "jot",
    "joy",
    "jp",
    "jpmorgan",
    "jprs",
    "juegos",
    "juniper",
    "kaufen",
    "kddi",
    "ke",
    "kerryhotels",
    "kerrylogistics",
    "kerryproperties",
    "kfh",
    "kg",
    "kh",
    "ki",
    "kia",
    "kids",
    "kim",
    "kindle",
    "kitchen",
    "kiwi",
    "km",
    "kn",
    "koeln",
    "komatsu",
    "kosher",
    "kp",
    "kpmg",
    "kpn",
    "kr",
    "krd",
    "kred",
    "kuokgroup",
    "kw",
    "ky",
    "kyoto",
    "kz",
    "la",
    "lacaixa",
    "lamborghini",
    "lamer",
    "lancaster",
    "land",
    "landrover",
    "lanxess",
    "lasalle",
    "lat",
    "latino",
    "latrobe",
    "law",
    "lawyer",
    "lb",
    "lc",
    "lds",
    "lease",
    "leclerc",
    "lefrak",
    "legal",
    "lego",
    "lexus",
    "lgbt",
    "li",
    "lidl",
    "life",
    "lifeinsurance",
    "lifestyle",
    "lighting",
    "like",
    "lilly",
    "limited",
    "limo",
    "lincoln",
    "link",
    "lipsy",
    "live",
    "living",
    "lk",
    "llc",
    "llp",
    "loan",
    "loans",
    "locker",
    "locus",
    "lol",
    "london",
    "lotte",
    "lotto",
    "love",
    "lpl",
    "lplfinancial",
    "lr",
    "ls",
    "lt",
    "ltd",
    "ltda",
    "lu",
    "lundbeck",
    "luxe",
    "luxury",
    "lv",
    "ly",
    "ma",
    "madrid",
    "maif",
    "maison",
    "makeup",
    "man",
    "management",
    "mango",
    "map",
    "market",
    "marketing",
    "markets",
    "marriott",
    "marshalls",
    "mattel",
    "mba",
    "mc",
    "mckinsey",
    "md",
    "me",
    "med",
    "media",
    "meet",
    "melbourne",
    "meme",
    "memorial",
    "men",
    "menu",
    "merckmsd",
    "mg",
    "mh",
    "miami",
    "microsoft",
    "mil",
    "mini",
    "mint",
    "mit",
    "mitsubishi",
    "mk",
    "ml",
    "mlb",
    "mls",
    "mm",
    "mma",
    "mn",
    "mo",
    "mobi",
    "mobile",
    "moda",
    "moe",
    "moi",
    "mom",
    "monash",
    "money",
    "monster",
    "mormon",
    "mortgage",
    "moscow",
    "moto",
    "motorcycles",
    "mov",
    "movie",
    "mp",
    "mq",
    "mr",
    "ms",
    "msd",
    "mt",
    "mtn",
    "mtr",
    "mu",
    "museum",
    "music",
    "mv",
    "mw",
    "mx",
    "my",
    "mz",
    "na",
    "nab",
    "nagoya",
    "name",
    "navy",
    "nba",
    "nc",
    "ne",
    "nec",
    "net",
    "netbank",
    "netflix",
    "network",
    "neustar",
    "new",
    "news",
    "next",
    "nextdirect",
    "nexus",
    "nf",
    "nfl",
    "ng",
    "ngo",
    "nhk",
    "ni",
    "nico",
    "nike",
    "nikon",
    "ninja",
    "nissan",
    "nissay",
    "nl",
    "no",
    "nokia",
    "norton",
    "now",
    "nowruz",
    "nowtv",
    "np",
    "nr",
    "nra",
    "nrw",
    "ntt",
    "nu",
    "nyc",
    "nz",
    "obi",
    "observer",
    "office",
    "okinawa",
    "olayan",
    "olayangroup",
    "ollo",
    "om",
    "omega",
    "one",
    "ong",
    "onion",
    "onl",
    "online",
    "ooo",
    "open",
    "oracle",
    "orange",
    "org",
    "organic",
    "origins",
    "osaka",
    "otsuka",
    "ott",
    "ovh",
    "pa",
    "page",
    "panasonic",
    "paris",
    "pars",
    "partners",
    "parts",
    "party",
    "pay",
    "pccw",
    "pe",
    "pet",
    "pf",
    "pfizer",
    "pg",
    "ph",
    "pharmacy",
    "phd",
    "philips",
    "phone",
    "photo",
    "photography",
    "photos",
    "physio",
    "pics",
    "pictet",
    "pictures",
    "pid",
    "pin",
    "ping",
    "pink",
    "pioneer",
    "pizza",
    "pk",
    "pl",
    "place",
    "play",
    "playstation",
    "plumbing",
    "plus",
    "pm",
    "pn",
    "pnc",
    "pohl",
    "poker",
    "politie",
    "porn",
    "post",
    "pr",
    "pramerica",
    "praxi",
    "press",
    "prime",
    "pro",
    "prod",
    "productions",
    "prof",
    "progressive",
    "promo",
    "properties",
    "property",
    "protection",
    "pru",
    "prudential",
    "ps",
    "pt",
    "pub",
    "pw",
    "pwc",
    "py",
    "qa",
    "qpon",
    "quebec",
    "quest",
    "racing",
    "radio",
    "re",
    "read",
    "realestate",
    "realtor",
    "realty",
    "recipes",
    "red",
    "redstone",
    "redumbrella",
    "rehab",
    "reise",
    "reisen",
    "reit",
    "reliance",
    "ren",
    "rent",
    "rentals",
    "repair",
    "report",
    "republican",
    "rest",
    "restaurant",
    "review",
    "reviews",
    "rexroth",
    "rich",
    "richardli",
    "ricoh",
    "ril",
    "rio",
    "rip",
    "ro",
    "rocks",
    "rodeo",
    "rogers",
    "room",
    "rs",
    "rsvp",
    "ru",
    "rugby",
    "ruhr",
    "run",
    "rw",
    "rwe",
    "ryukyu",
    "sa",
    "saarland",
    "safe",
    "safety",
    "sakura",
    "sale",
    "salon",
    "samsclub",
    "samsung",
    "sandvik",
    "sandvikcoromant",
    "sanofi",
    "sap",
    "sarl",
    "sas",
    "save",
    "saxo",
    "sb",
    "sbi",
    "sbs",
    "sc",
    "scb",
    "schaeffler",
    "schmidt",
    "scholarships",
    "school",
    "schule",
    "schwarz",
    "science",
    "scot",
    "sd",
    "se",
    "search",
    "seat",
    "secure",
    "security",
    "seek",
    "select",
    "sener",
    "services",
    "seven",
    "sew",
    "sex",
    "sexy",
    "sfr",
    "sg",
    "sh",
    "shangrila",
    "sharp",
    "shell",
    "shia",
    "shiksha",
    "shoes",
    "shop",
    "shopping",
    "shouji",
    "show",
    "si",
    "silk",
    "sina",
    "singles",
    "site",
    "sj",
    "sk",
    "ski",
    "skin",
    "sky",
    "skype",
    "sl",
    "sling",
    "sm",
    "smart",
    "smile",
    "sn",
    "sncf",
    "so",
    "soccer",
    "social",
    "softbank",
    "software",
    "sohu",
    "solar",
    "solutions",
    "song",
    "sony",
    "soy",
    "spa",
    "space",
    "sport",
    "spot",
    "sr",
    "srl",
    "ss",
    "st",
    "stada",
    "staples",
    "star",
    "statebank",
    "statefarm",
    "stc",
    "stcgroup",
    "stockholm",
    "storage",
    "store",
    "stream",
    "studio",
    "study",
    "style",
    "su",
    "sucks",
    "supplies",
    "supply",
    "support",
    "surf",
    "surgery",
    "suzuki",
    "sv",
    "swatch",
    "swiss",
    "sx",
    "sy",
    "sydney",
    "systems",
    "sz",
    "tab",
    "taipei",
    "talk",
    "taobao",
    "target",
    "tatamotors",
    "tatar",
    "tattoo",
    "tax",
    "taxi",
    "tc",
    "tci",
    "td",
    "tdk",
    "team",
    "tech",
    "technology",
    "tel",
    "temasek",
    "tennis",
    "teva",
    "tf",
    "tg",
    "th",
    "thd",
    "theater",
    "theatre",
    "tiaa",
    "tickets",
    "tienda",
    "tips",
    "tires",
    "tirol",
    "tj",
    "tjmaxx",
    "tjx",
    "tk",
    "tkmaxx",
    "tl",
    "tm",
    "tmall",
    "tn",
    "to",
    "today",
    "tokyo",
    "ton",
    "tools",
    "top",
    "toray",
    "toshiba",
    "total",
    "tours",
    "town",
    "toyota",
    "toys",
    "tr",
    "trade",
    "trading",
    "training",
    "travel",
    "travelers",
    "travelersinsurance",
    "trust",
    "trv",
    "tt",
    "tube",
    "tui",
    "tunes",
    "tushu",
    "tv",
    "tvs",
    "tw",
    "tz",
    "ua",
    "ubank",
    "ubs",
    "ug",
    "uk",
    "unicom",
    "university",
    "uno",
    "uol",
    "ups",
    "us",
    "uy",
    "uz",
    "va",
    "vacations",
    "vana",
    "vanguard",
    "vc",
    "ve",
    "vegas",
    "ventures",
    "verisign",
    "vermögensberater",
    "vermögensberatung",
    "versicherung",
    "vet",
    "vg",
    "vi",
    "viajes",
    "video",
    "vig",
    "viking",
    "villas",
    "vin",
    "vip",
    "virgin",
    "visa",
    "vision",
    "viva",
    "vivo",
    "vlaanderen",
    "vn",
    "vodka",
    "volvo",
    "vote",
    "voting",
    "voto",
    "voyage",
    "vu",
    "wales",
    "walmart",
    "walter",
    "wang",
    "wanggou",
    "watch",
    "watches",
    "weather",
    "weatherchannel",
    "webcam",
    "weber",
    "website",
    "wed",
    "wedding",
    "weibo",
    "weir",
    "wf",
    "whoswho",
    "wien",
    "wiki",
    "williamhill",
    "win",
    "windows",
    "wine",
    "winners",
    "wme",
    "wolterskluwer",
    "woodside",
    "work",
    "works",
    "world",
    "wow",
    "ws",
    "wtc",
    "wtf",
    "xbox",
    "xerox",
    "xihuan",
    "xin",
    "xxx",
    "xyz",
    "yachts",
    "yahoo",
    "yamaxun",
    "yandex",
    "ye",
    "yodobashi",
    "yoga",
    "yokohama",
    "you",
    "youtube",
    "yt",
    "yun",
    "za",
    "zappos",
    "zara",
    "zero",
    "zip",
    "zm",
    "zone",
    "zuerich",
    "zw",
    "ελ",
    "ευ",
    "бг",
    "бел",
    "дети",
    "ею",
    "католик",
    "ком",
    "мкд",
    "мон",
    "москва",
    "онлайн",
    "орг",
    "рус",
    "рф",
    "сайт",
    "срб",
    "укр",
    "қаз",
    "հայ",
    "ישראל",
    "קום",
    "ابوظبي",
    "ارامكو",
    "الاردن",
    "البحرين",
    "الجزائر",
    "السعودية",
    "العليان",
    "المغرب",
    "امارات",
    "ایران",
    "بارت",
    "بازار",
    "بيتك",
    "بھارت",
    "تونس",
    "سودان",
    "سورية",
    "شبكة",
    "عراق",
    "عرب",
    "عمان",
    "فلسطين",
    "قطر",
    "كاثوليك",
    "كوم",
    "مصر",
    "مليسيا",
    "موريتانيا",
    "موقع",
    "همراه",
    "پاکستان",
    "ڀارت",
    "कॉम",
    "नेट",
    "भारत",
    "भारतम्",
    "भारोत",
    "संगठन",
    "বাংলা",
    "ভারত",
    "ভাৰত",
    "ਭਾਰਤ",
    "ભારત",
    "ଭାରତ",
    "இந்தியா",
    "இலங்கை",
    "சிங்கப்பூர்",
    "భారత్",
    "ಭಾರತ",
    "ഭാരതം",
    "ලංකා",
    "คอม",
    "ไทย",
    "ລາວ",
    "გე",
    "みんな",
    "アマゾン",
    "クラウド",
    "グーグル",
    "コム",
    "ストア",
    "セール",
    "ファッション",
    "ポイント",
    "世界",
    "中信",
    "中国",
    "中國",
    "中文网",
    "亚马逊",
    "企业",
    "佛山",
    "信息",
    "健康",
    "八卦",
    "公司",
    "公益",
    "台湾",
    "台灣",
    "商城",
    "商店",
    "商标",
    "嘉里",
    "嘉里大酒店",
    "在线",
    "大拿",
    "天主教",
    "娱乐",
    "家電",
    "广东",
    "微博",
    "慈善",
    "我爱你",
    "手机",
    "招聘",
    "政务",
    "政府",
    "新加坡",
    "新闻",
    "时尚",
    "書籍",
    "机构",
    "淡马锡",
    "游戏",
    "澳門",
    "点看",
    "移动",
    "组织机构",
    "网址",
    "网店",
    "网站",
    "网络",
    "联通",
    "谷歌",
    "购物",
    "通販",
    "集团",
    "電訊盈科",
    "飞利浦",
    "食品",
    "餐厅",
    "香格里拉",
    "香港",
    "닷넷",
    "닷컴",
    "삼성",
    "한국",
];

fn common_tlds() -> &'static std::collections::HashSet<&'static str> {
    static TLDS: std::sync::OnceLock<std::collections::HashSet<&'static str>> =
        std::sync::OnceLock::new();
    TLDS.get_or_init(|| COMMON_TLDS.iter().copied().collect())
}

/// `is_common_tld` (MessageEntity.cpp:1060-1198).
fn is_common_tld(tld: &str) -> bool {
    let is_lower = tld.bytes().all(|c| c.is_ascii_lowercase());
    if is_lower {
        // fast path
        return common_tlds().contains(tld);
    }
    let str_lower = tdutils::utf8::utf8_to_lower(tld);
    if str_lower != tld
        && tdutils::utf8::utf8_substr(&str_lower, 1) == tdutils::utf8::utf8_substr(tld, 1)
    {
        return false;
    }
    common_tlds().contains(str_lower.as_str())
}

/// `fix_url` (MessageEntity.cpp:1199-1345): the empty string means the URL
/// was rejected (the C++ returns an empty `Slice`).
fn fix_url(url: &str) -> &str {
    let mut full_len = url.len();
    let mut rest = url;

    let mut has_protocol = false;
    // The C++ `to_lower(str.substr(0, 10))` prefix checks, done on bytes:
    // `Slice::substr` may cut a UTF-8 code point in half (Rust `&str`
    // slicing would panic), and `utf8_to_lower` never maps a non-ASCII
    // byte of a valid UTF-8 input to an ASCII letter, so ASCII-case-
    // insensitive byte comparison is exact.
    let str_begin = &rest.as_bytes()[..rest.len().min(10)];
    if str_begin.len() >= 7 && str_begin[..7].eq_ignore_ascii_case(b"http://")
        || str_begin.len() >= 8 && str_begin[..8].eq_ignore_ascii_case(b"https://")
        || str_begin.len() >= 6 && str_begin[..6].eq_ignore_ascii_case(b"ftp://")
        || str_begin.len() >= 10 && str_begin[..10].eq_ignore_ascii_case(b"tonsite://")
    {
        let pos = rest.find(':').unwrap();
        rest = &rest[pos + 3..];
        has_protocol = true;
    }
    let domain_end = rest
        .find('/')
        .unwrap_or(rest.len())
        .min(rest.find('?').unwrap_or(rest.len()))
        .min(rest.find('#').unwrap_or(rest.len()));
    let mut domain = &rest[..domain_end];
    let path = &rest[domain_end..];

    if let Some(at_pos) = domain.find('@') {
        domain = &domain[at_pos + 1..];
    }
    // `Slice::truncate` keeps the slice when `rfind` fails (npos is not
    // `len_ > size`).
    if let Some(colon_pos) = domain.rfind(':') {
        domain = &domain[..colon_pos];
    }

    if domain.len() == 12 && (domain.starts_with('t') || domain.starts_with('T')) {
        let domain_lower = domain.to_ascii_lowercase();
        if domain_lower == "teiegram.org" {
            return "";
        }
    }

    let mut balance = [0i32; 3];
    let path_bytes = path.as_bytes();
    let mut path_pos = 0usize;
    while path_pos < path.len() {
        match path_bytes[path_pos] {
            b'(' => balance[0] += 1,
            b'[' => balance[1] += 1,
            b'{' => balance[2] += 1,
            b')' => balance[0] -= 1,
            b']' => balance[1] -= 1,
            b'}' => balance[2] -= 1,
            _ => {}
        }
        if balance[0] < 0 || balance[1] < 0 || balance[2] < 0 {
            break;
        }
        path_pos += 1;
    }
    while path_pos > 0 && BAD_PATH_END_CHARS.contains(&path_bytes[path_pos - 1]) {
        path_pos -= 1;
    }
    full_len -= path.len() - path_pos;

    let domain_bytes = domain.as_bytes();
    let mut prev = 0usize;
    let mut domain_part_count = 0usize;
    let mut has_non_digit = false;
    let mut is_ipv4 = true;
    for i in 0..=domain.len() {
        if i == domain.len() || domain_bytes[i] == b'.' {
            let part_size = i - prev;
            if part_size == 0 || part_size >= 64 || domain_bytes[i - 1] == b'-' {
                return "";
            }
            if is_ipv4 {
                if part_size > 3 {
                    is_ipv4 = false;
                }
                if part_size == 3
                    && (domain_bytes[prev] >= b'3'
                        || (domain_bytes[prev] == b'2'
                            && (domain_bytes[prev + 1] >= b'6'
                                || (domain_bytes[prev + 1] == b'5'
                                    && domain_bytes[prev + 2] >= b'6'))))
                {
                    is_ipv4 = false;
                }
                if domain_bytes[prev] == b'0' && part_size >= 2 {
                    is_ipv4 = false;
                }
            }

            domain_part_count += 1;
            if i != domain.len() {
                prev = i + 1;
            }
        } else if !domain_bytes[i].is_ascii_digit() {
            is_ipv4 = false;
            has_non_digit = true;
        }
    }
    if domain_part_count == 1 {
        return "";
    }

    if is_ipv4 && domain_part_count == 4 {
        return &url[..full_len];
    }

    if !has_non_digit {
        return "";
    }

    let tld = &domain[prev..];
    if tdutils::utf8::utf8_length(tld) <= 1 {
        return "";
    }

    if tld.starts_with("xn--") {
        if tld.len() <= 5 {
            return "";
        }
        if !tld.as_bytes()[4..]
            .iter()
            .all(|&c| is_alpha_digit(u32::from(c)))
        {
            return "";
        }
    } else {
        if tld.contains('_') {
            return "";
        }
        if tld.contains('-') {
            return "";
        }

        if !has_protocol && !is_common_tld(tld) {
            return "";
        }
    }

    assert!(prev > 0, "the C++ CHECK(prev > 0)");
    let mut prev = prev - 1;
    while prev > 0 {
        prev -= 1;
        if domain_bytes[prev] == b'_' {
            return "";
        } else if domain_bytes[prev] == b'.' {
            break;
        }
    }

    &url[..full_len]
}

/// `find_tg_urls` (MessageEntity.cpp:1386-1388).
pub fn find_tg_urls(s: &str) -> Vec<&str> {
    match_tg_urls(s)
}

/// `find_urls` (MessageEntity.cpp:1390-1406): the bool is the C++
/// `is_email_address` pair flag.
pub fn find_urls(s: &str) -> Vec<(&str, bool)> {
    let mut result = Vec::new();
    for url in match_urls(s) {
        if is_email_address(url) {
            result.push((url, true));
        } else if url.starts_with("mailto:") && is_email_address(&url[7..]) {
            result.push((&url[7..], true));
        } else {
            let url = fix_url(url);
            if !url.is_empty() {
                result.push((url, false));
            }
        }
    }
    result
}

/// `text_length` (MessageEntity.cpp:1463-1465).
fn text_length(text: &str) -> i32 {
    tdutils::utf8::utf8_utf16_length(text) as i32
}

/// `is_plain_domain` (MessageEntity.cpp:1829-1831).
fn is_plain_domain(url: &str) -> bool {
    !url.contains('/') && !url.contains('?') && !url.contains('#')
}

/// `get_first_url` (MessageEntity.cpp:1833-1905): the first Url/TextUrl
/// entity whose target is not a tg/ton/ftp scheme or a plain domain.
pub fn get_first_url(text: &FormattedText) -> &str {
    for entity in &text.entities {
        match entity.r#type {
            Some(Type::Url) => {
                if entity.length <= 4 {
                    continue;
                }
                if entity.offset < 0 {
                    // A negative offset converts to a huge `size_t` in the
                    // C++ and yields an empty URL, which `is_plain_domain`
                    // rejects.
                    continue;
                }
                let url = tdutils::utf8::utf8_utf16_substr_range(
                    &text.text,
                    entity.offset as usize,
                    entity.length as usize,
                );
                // The C++ `to_lower(url.substr(0, 8))` scheme checks, done
                // on bytes for the same reason as in `fix_url`.
                let scheme = &url.as_bytes()[..url.len().min(8)];
                if scheme.eq_ignore_ascii_case(b"ton:")
                    || (scheme.len() >= 3 && scheme[..3].eq_ignore_ascii_case(b"tg:"))
                    || scheme.eq_ignore_ascii_case(b"ftp:")
                    || scheme.eq_ignore_ascii_case(b"tonsite:")
                    || is_plain_domain(url)
                {
                    continue;
                }
                return url;
            }
            Some(Type::TextUrl) => {
                let url = entity.argument.as_str();
                let scheme = &url.as_bytes()[..url.len().min(8)];
                if scheme.eq_ignore_ascii_case(b"ton:")
                    || (scheme.len() >= 3 && scheme[..3].eq_ignore_ascii_case(b"tg:"))
                    || scheme.eq_ignore_ascii_case(b"ftp:")
                    || scheme.eq_ignore_ascii_case(b"tonsite:")
                {
                    continue;
                }
                return url;
            }
            // Every other C++ switch arm is a `break`.
            Some(_) => {}
            None => unreachable!("entities in flight always carry a real type"),
        }
    }

    ""
}

/// `is_visible_url` (MessageEntity.cpp:1907-1927).
pub fn is_visible_url(text: &FormattedText, url: &str) -> bool {
    if url.is_empty() {
        return false;
    }

    let url_size = text_length(url);
    let mut cur_offset = 0i32;
    let mut left_text: &str = &text.text;
    for entity in &text.entities {
        if entity.r#type == Some(Type::Url) && url_size == entity.length {
            assert!(
                entity.offset >= cur_offset,
                "the C++ CHECK(entity.offset >= cur_offset)"
            );
            left_text =
                tdutils::utf8::utf8_utf16_substr(left_text, (entity.offset - cur_offset) as usize);
            cur_offset = entity.offset;
            if left_text.starts_with(url) {
                return true;
            }
        }
    }

    false
}

// ===== M.8.5 — validation, ordering, merging (MessageEntity.cpp:1464-1827) =====

/// The `entity->type` of an entity in flight; the C++ default sentinel
/// `Type::Size` never reaches the fixup machinery.
fn entity_type(entity: &MessageEntity) -> Type {
    entity
        .r#type
        .expect("entities in flight always carry a real type")
}

/// `sort_entities` (MessageEntity.cpp:1467-1473).
fn sort_entities(entities: &mut [MessageEntity]) {
    if !entities.is_sorted() {
        entities.sort();
    }
}

/// `check_is_sorted` (MessageEntity.cpp:1475-1480) — the C++ `LOG_CHECK` is
/// `TD_DEBUG`-gated, so a side-effect-free `debug_assert!` is exact.
fn check_is_sorted(entities: &[MessageEntity]) {
    debug_assert!(
        entities.windows(2).all(|w| w[0] <= w[1]),
        "the C++ LOG_CHECK(std::is_sorted(entities))"
    );
}

/// `check_non_intersecting` (MessageEntity.cpp:1482-1487).
fn check_non_intersecting(entities: &[MessageEntity]) {
    debug_assert!(
        entities
            .windows(2)
            .all(|w| w[0].offset + w[0].length <= w[1].offset),
        "the C++ LOG_CHECK(entities[i].offset + entities[i].length <= entities[i + 1].offset)"
    );
}

/// `are_entities_valid` (MessageEntity.cpp:1559-1626).
fn are_entities_valid(entities: &[MessageEntity]) -> bool {
    if entities.is_empty() {
        return true;
    }
    check_is_sorted(entities);

    let mut end_pos = [-1i32; SPLITTABLE_ENTITY_TYPE_COUNT];
    let mut nested_entities_stack: Vec<&MessageEntity> = Vec::new();
    let mut nested_entity_type_mask = 0i32;
    for entity in entities {
        while let Some(top) = nested_entities_stack.last().copied() {
            if entity.offset >= top.offset + top.length {
                // remove non-intersecting entities from the stack
                nested_entity_type_mask -= entity_type_mask(entity_type(top));
                nested_entities_stack.pop();
            } else {
                break;
            }
        }

        if let Some(top) = nested_entities_stack.last().copied() {
            if entity.offset + entity.length > top.offset + top.length {
                // entity intersects some previous entity
                return false;
            }
            if (nested_entity_type_mask & entity_type_mask(entity_type(entity))) != 0 {
                // entity has the same type as one of the previous nested
                return false;
            }
            let parent_type = entity_type(top);
            if is_pre_entity(parent_type) {
                // Pre and Code can't contain nested entities
                return false;
            }
            // parents are not pre after this point
            if is_pre_entity(entity_type(entity))
                && (nested_entity_type_mask & !blockquote_entities_mask()) != 0
            {
                // Pre and Code can't be part of other entities, except blockquote
                return false;
            }
            if (is_continuous_entity(entity_type(entity))
                || is_blockquote_entity(entity_type(entity)))
                && (nested_entity_type_mask & continuous_entities_mask()) != 0
            {
                // continuous and blockquote can't be part of other continuous entity
                return false;
            }
            if is_blockquote_entity(entity_type(entity))
                && (nested_entity_type_mask & blockquote_entities_mask()) != 0
            {
                // blockquote entities can't be nested
                return false;
            }
            if (nested_entity_type_mask & splittable_entities_mask()) != 0 {
                // the previous nested entity may be needed to split for consistency
                // alternatively, better entity merging needs to be implemented
                return false;
            }
        }

        if is_splittable_entity(entity_type(entity)) {
            let index = splittable_entity_type_index(entity_type(entity));
            if end_pos[index] >= entity.offset {
                // the entities can be merged
                return false;
            }
            end_pos[index] = entity.offset + entity.length;
        }
        nested_entities_stack.push(entity);
        nested_entity_type_mask += entity_type_mask(entity_type(entity));
    }
    true
}

/// `remove_intersecting_entities` (MessageEntity.cpp:1628-1644): removes all
/// intersecting entities, including nested.
fn remove_intersecting_entities(entities: &mut Vec<MessageEntity>) {
    check_is_sorted(entities);
    let mut last_entity_end = 0i32;
    let mut left_entities = 0usize;
    for i in 0..entities.len() {
        assert!(
            entities[i].length > 0,
            "the C++ CHECK(entities[i].length > 0)"
        );
        if entities[i].offset >= last_entity_end {
            last_entity_end = entities[i].offset + entities[i].length;
            if i != left_entities {
                entities.swap(left_entities, i);
            }
            left_entities += 1;
        }
    }
    entities.truncate(left_entities);
}

/// `remove_entities_intersecting_blockquote` (MessageEntity.cpp:1646-1677):
/// `entities` and `blockquote_entities` must be pre-sorted and non-overlapping.
fn remove_entities_intersecting_blockquote(
    entities: &mut Vec<MessageEntity>,
    blockquote_entities: &[MessageEntity],
) {
    check_non_intersecting(entities);
    check_non_intersecting(blockquote_entities);
    if blockquote_entities.is_empty() {
        // fast path
        return;
    }

    let mut blockquote_index = 0usize;
    let mut left_entities = 0usize;
    for i in 0..entities.len() {
        while blockquote_index != blockquote_entities.len()
            && (!is_blockquote_entity(entity_type(&blockquote_entities[blockquote_index]))
                || blockquote_entities[blockquote_index].offset
                    + blockquote_entities[blockquote_index].length
                    <= entities[i].offset)
        {
            blockquote_index += 1;
        }
        if blockquote_index != blockquote_entities.len() {
            let blockquote = &blockquote_entities[blockquote_index];
            let blockquote_end = blockquote.offset + blockquote.length;
            let entity_end = entities[i].offset + entities[i].length;
            if blockquote_end < entity_end
                || (entities[i].offset < blockquote.offset && blockquote.offset < entity_end)
            {
                continue;
            }
        }
        if i != left_entities {
            entities.swap(left_entities, i);
        }
        left_entities += 1;
    }
    entities.truncate(left_entities);
}

/// `fix_entity_offsets` (MessageEntity.cpp:1679-1721): keeps only
/// non-intersecting entities and fixes entity offsets from UTF-8 to UTF-16.
fn fix_entity_offsets(text: &str, entities: &mut Vec<MessageEntity>) {
    if entities.is_empty() {
        return;
    }

    sort_entities(entities);

    remove_intersecting_entities(entities);

    let bytes = text.as_bytes();
    let begin = 0usize;
    let mut ptr = begin;
    let end = bytes.len();

    let mut utf16_pos = 0i32;
    for entity in entities.iter_mut() {
        let mut cnt = 2;
        let entity_begin = entity.offset;
        let entity_end = entity.offset + entity.length;

        let mut pos = (ptr - begin) as i32;
        if entity_begin == pos {
            cnt -= 1;
            entity.offset = utf16_pos;
        }

        while ptr != end && cnt > 0 {
            let c = bytes[ptr];
            utf16_pos += 1 + i32::from(c >= 0xf0);
            let (_, width) = tdutils::utf8::next_utf8_unsafe(&bytes[ptr..]);
            ptr += width;

            pos = (ptr - begin) as i32;
            if entity_begin == pos {
                cnt -= 1;
                entity.offset = utf16_pos;
            } else if entity_end == pos {
                cnt -= 1;
                entity.length = utf16_pos - entity.offset;
            }
        }
        assert!(cnt == 0, "the C++ CHECK(cnt == 0)");
    }
}

/// `is_found_entity_type` (MessageEntity.cpp:1723-1740).
pub fn is_found_entity_type(
    r#type: Type,
    skip_bot_commands: bool,
    skip_media_timestamps: bool,
) -> bool {
    match r#type {
        Type::Mention
        | Type::Hashtag
        | Type::Cashtag
        | Type::PhoneNumber
        | Type::BankCardNumber
        | Type::Url
        | Type::EmailAddress => true,
        Type::BotCommand => !skip_bot_commands,
        Type::MediaTimestamp => !skip_media_timestamps,
        _ => false,
    }
}

/// The offset of a matched subslice in bytes (the C++
/// `entity.begin() - text.begin()`).
fn slice_offset(text: &str, slice: &str) -> i32 {
    (slice.as_ptr() as usize - text.as_ptr() as usize) as i32
}

/// `find_entities` (MessageEntity.cpp:1742-1781) — the aggregate extractor
/// behind `getTextEntities`.
pub fn find_entities(
    text: &str,
    skip_bot_commands: bool,
    skip_media_timestamps: bool,
) -> Vec<MessageEntity> {
    let mut entities: Vec<MessageEntity> = Vec::new();

    let mut add_entities = |r#type: Type, found: Vec<&str>| {
        for entity in found {
            entities.push(MessageEntity::new(
                r#type,
                slice_offset(text, entity),
                entity.len() as i32,
            ));
        }
    };
    add_entities(Type::Mention, find_mentions(text));
    if !skip_bot_commands {
        add_entities(Type::BotCommand, find_bot_commands(text));
    }
    add_entities(Type::Hashtag, find_hashtags(text));
    add_entities(Type::Cashtag, find_cashtags(text));
    // TODO find_phone_numbers
    add_entities(Type::BankCardNumber, find_bank_card_numbers(text));
    add_entities(Type::Url, find_tg_urls(text));
    for (url, is_email) in find_urls(text) {
        let r#type = if is_email {
            Type::EmailAddress
        } else {
            Type::Url
        };
        entities.push(MessageEntity::new(
            r#type,
            slice_offset(text, url),
            url.len() as i32,
        ));
    }
    if !skip_media_timestamps {
        for (media_timestamp, seconds) in find_media_timestamps(text) {
            entities.push(MessageEntity::media_timestamp(
                slice_offset(text, media_timestamp),
                media_timestamp.len() as i32,
                seconds,
            ));
        }
    }

    fix_entity_offsets(text, &mut entities);

    entities
}

/// `find_media_timestamp_entities` (MessageEntity.cpp:1783-1796).
fn find_media_timestamp_entities(text: &str) -> Vec<MessageEntity> {
    let mut entities = Vec::new();

    for (media_timestamp, seconds) in find_media_timestamps(text) {
        entities.push(MessageEntity::media_timestamp(
            slice_offset(text, media_timestamp),
            media_timestamp.len() as i32,
            seconds,
        ));
    }

    fix_entity_offsets(text, &mut entities);

    entities
}

/// `merge_entities` (MessageEntity.cpp:1798-1827).
fn merge_entities(
    old_entities: Vec<MessageEntity>,
    new_entities: Vec<MessageEntity>,
) -> Vec<MessageEntity> {
    if new_entities.is_empty() {
        return old_entities;
    }
    if old_entities.is_empty() {
        return new_entities;
    }

    let mut result = Vec::with_capacity(old_entities.len() + new_entities.len());

    let mut new_it = new_entities.into_iter();
    for old_entity in old_entities {
        while new_it
            .as_slice()
            .first()
            .is_some_and(|new_entity| new_entity.offset + new_entity.length <= old_entity.offset)
        {
            result.push(new_it.next().unwrap());
        }
        let old_entity_end = old_entity.offset + old_entity.length;
        result.push(old_entity);
        while new_it
            .as_slice()
            .first()
            .is_some_and(|new_entity| new_entity.offset < old_entity_end)
        {
            new_it.next();
        }
    }
    result.extend(new_it);

    result
}

// ===== M.8.2 (cont.) — fix_entities stack + FormattedText fixers
//        (MessageEntity.cpp:4089-4912, 3579-3667) =====

/// `split_entities` (MessageEntity.cpp:4315-4386): `entities` must contain
/// only splittable entities.
fn split_entities(entities: &mut Vec<MessageEntity>, other_entities: &[MessageEntity]) {
    check_is_sorted(entities);
    check_is_sorted(other_entities);

    struct Splitter<'a> {
        entities: &'a [MessageEntity],
        other_entities: &'a [MessageEntity],
        result: Vec<MessageEntity>,
        begin_pos: [i32; SPLITTABLE_ENTITY_TYPE_COUNT],
        end_pos: [i32; SPLITTABLE_ENTITY_TYPE_COUNT],
        it: usize,
        nested_entities_stack: Vec<usize>,
    }

    impl Splitter<'_> {
        fn flush_entities(&mut self, offset: i32) {
            for r#type in [
                Type::Bold,
                Type::Italic,
                Type::Underline,
                Type::Strikethrough,
                Type::Spoiler,
            ] {
                let index = splittable_entity_type_index(r#type);
                if self.end_pos[index] != 0 && self.begin_pos[index] < offset {
                    if self.end_pos[index] <= offset {
                        self.result.push(MessageEntity::new(
                            r#type,
                            self.begin_pos[index],
                            self.end_pos[index] - self.begin_pos[index],
                        ));
                        self.begin_pos[index] = 0;
                        self.end_pos[index] = 0;
                    } else {
                        self.result.push(MessageEntity::new(
                            r#type,
                            self.begin_pos[index],
                            offset - self.begin_pos[index],
                        ));
                        self.begin_pos[index] = offset;
                    }
                }
            }
        }

        fn add_entities(&mut self, end_offset: i32) {
            while self.it != self.entities.len() {
                if self.entities[self.it].offset >= end_offset {
                    break;
                }
                let entity = &self.entities[self.it];
                assert!(
                    is_splittable_entity(entity_type(entity)),
                    "the C++ CHECK(is_splittable_entity(it->type))"
                );
                let index = splittable_entity_type_index(entity_type(entity));
                if entity.offset <= self.end_pos[index] && self.end_pos[index] != 0 {
                    if entity.offset + entity.length > self.end_pos[index] {
                        self.end_pos[index] = entity.offset + entity.length;
                    }
                } else {
                    self.flush_entities(entity.offset);
                    self.begin_pos[index] = entity.offset;
                    self.end_pos[index] = entity.offset + entity.length;
                }
                self.it += 1;
            }
            self.flush_entities(end_offset);
        }

        fn add_offset(&mut self, offset: i32) {
            while let Some(&top) = self.nested_entities_stack.last() {
                if offset >= self.other_entities[top].offset + self.other_entities[top].length {
                    // remove non-intersecting entities from the stack
                    let old_size = self.result.len();
                    let end = self.other_entities[top].offset + self.other_entities[top].length;
                    self.add_entities(end);
                    if is_pre_entity(entity_type(&self.other_entities[top])) {
                        self.result.truncate(old_size);
                    }
                    self.nested_entities_stack.pop();
                } else {
                    break;
                }
            }

            self.add_entities(offset);
        }
    }

    let mut splitter = Splitter {
        entities,
        other_entities,
        result: Vec::new(),
        begin_pos: [0; SPLITTABLE_ENTITY_TYPE_COUNT],
        end_pos: [0; SPLITTABLE_ENTITY_TYPE_COUNT],
        it: 0,
        nested_entities_stack: Vec::new(),
    };
    for (i, entity) in other_entities.iter().enumerate() {
        splitter.add_offset(entity.offset);
        splitter.nested_entities_stack.push(i);
    }
    splitter.add_offset(i32::MAX);

    *entities = std::mem::take(&mut splitter.result);

    // entities are sorted only by offset now, re-sort if needed
    sort_entities(entities);
}

/// `resplit_entities` (MessageEntity.cpp:4388-4401).
fn resplit_entities(
    mut splittable_entities: Vec<MessageEntity>,
    mut entities: Vec<MessageEntity>,
) -> Vec<MessageEntity> {
    if !splittable_entities.is_empty() {
        split_entities(&mut splittable_entities, &entities); // can merge some entities

        if entities.is_empty() {
            return splittable_entities;
        }

        entities.extend(splittable_entities);
        sort_entities(&mut entities);
    }
    entities
}

/// `fix_entities` (MessageEntity.cpp:4403-4438).
pub fn fix_entities(entities: &mut Vec<MessageEntity>) {
    sort_entities(entities);

    if are_entities_valid(entities) {
        // fast path
        return;
    }

    let mut continuous_entities = Vec::new();
    let mut blockquote_entities = Vec::new();
    let mut splittable_entities = Vec::new();
    for entity in entities.drain(..) {
        if is_splittable_entity(entity_type(&entity)) {
            splittable_entities.push(entity);
        } else if is_blockquote_entity(entity_type(&entity)) {
            blockquote_entities.push(entity);
        } else {
            continuous_entities.push(entity);
        }
    }
    remove_intersecting_entities(&mut continuous_entities); // continuous entities can't intersect each other

    if !blockquote_entities.is_empty() {
        remove_intersecting_entities(&mut blockquote_entities); // blockquote entities can't intersect each other

        // blockquote entities can contain continuous entities, but can't intersect them in the other ways
        remove_entities_intersecting_blockquote(&mut continuous_entities, &blockquote_entities);

        continuous_entities.extend(blockquote_entities);
        sort_entities(&mut continuous_entities);
    }

    // must be called once to not merge some adjacent entities
    *entities = resplit_entities(splittable_entities, continuous_entities);
    check_is_sorted(entities);
}

/// `merge_new_entities` (MessageEntity.cpp:4440-4475).
fn merge_new_entities(entities: &mut Vec<MessageEntity>, new_entities: Vec<MessageEntity>) {
    check_is_sorted(entities);
    if new_entities.is_empty() {
        // fast path
        return;
    }

    check_non_intersecting(&new_entities);

    let mut continuous_entities = Vec::new();
    let mut blockquote_entities = Vec::new();
    let mut splittable_entities = Vec::new();
    for entity in entities.drain(..) {
        if is_splittable_entity(entity_type(&entity)) {
            splittable_entities.push(entity);
        } else if is_blockquote_entity(entity_type(&entity)) {
            blockquote_entities.push(entity);
        } else {
            continuous_entities.push(entity);
        }
    }

    let mut new_entities = new_entities;
    remove_entities_intersecting_blockquote(&mut new_entities, &blockquote_entities);

    // merge before combining with blockquote entities
    continuous_entities = merge_entities(continuous_entities, new_entities);

    if !blockquote_entities.is_empty() {
        continuous_entities.extend(blockquote_entities);
        sort_entities(&mut continuous_entities);
    }

    // must be called once to not merge some adjacent entities
    *entities = resplit_entities(splittable_entities, continuous_entities);
    check_is_sorted(entities);
}

/// `clean_input_string_with_entities` (MessageEntity.cpp:4124-4268): like
/// `clean_input_string` but also fixes entities; they must be sorted, can
/// be nested, but must not intersect each other.
fn clean_input_string_with_entities(
    text: &str,
    entities: &mut [MessageEntity],
) -> Result<String, Status> {
    check_is_sorted(entities);

    struct EntityInfo {
        index: usize,
        utf16_skipped_before: i32,
    }
    let mut nested_entities_stack: Vec<EntityInfo> = Vec::new();
    let mut current_entity = 0usize;

    let mut utf16_offset = 0i32;
    let mut utf16_skipped = 0i32;

    let text_size = text.len();
    let bytes = text.as_bytes();

    let mut result: Vec<u8> = Vec::with_capacity(text_size);
    let mut pos = 0usize;
    while pos <= text_size {
        // The C++ reads `text[text_size]` as the terminating '\0'; the
        // virtual character only feeds the first-code-unit test below,
        // because the loop breaks before the character switch at the end.
        let c = bytes.get(pos).copied().unwrap_or(0);
        let is_utf8_character_begin = tdutils::utf8::is_utf8_character_first_code_unit(c);
        if is_utf8_character_begin {
            while let Some(top) = nested_entities_stack.last() {
                let entity = &entities[top.index];
                let entity_end = entity.offset + entity.length;
                if utf16_offset < entity_end {
                    break;
                }

                if utf16_offset != entity_end {
                    assert_eq!(
                        utf16_offset,
                        entity_end + 1,
                        "the C++ CHECK(utf16_offset == entity_end + 1)"
                    );
                    return Err(Status::error(
                        400,
                        &format!(
                            "Entity beginning at UTF-16 offset {} ends in a middle of a UTF-16 symbol at byte offset {}",
                            entity.offset, pos
                        ),
                    ));
                }

                let skipped_before_current_entity = top.utf16_skipped_before;
                let entity = &mut entities[top.index];
                entity.offset -= skipped_before_current_entity;
                entity.length -= utf16_skipped - skipped_before_current_entity;
                nested_entities_stack.pop();
            }
            while current_entity < entities.len() && utf16_offset >= entities[current_entity].offset
            {
                if utf16_offset != entities[current_entity].offset {
                    assert_eq!(
                        utf16_offset,
                        entities[current_entity].offset + 1,
                        "the C++ CHECK(utf16_offset == offset + 1)"
                    );
                    return Err(Status::error(
                        400,
                        &format!(
                            "Entity begins in a middle of a UTF-16 symbol at byte offset {}",
                            pos
                        ),
                    ));
                }
                nested_entities_stack.push(EntityInfo {
                    index: current_entity,
                    utf16_skipped_before: utf16_skipped,
                });
                current_entity += 1;
            }
        }
        if pos == text_size {
            break;
        }

        match c {
            // remove control characters
            0..=9 | 11 | 12 | 14..=32 => {
                result.push(b' ');
                utf16_offset += 1;
            }
            b'\r' => {
                // skip
                utf16_offset += 1;
                utf16_skipped += 1;
            }
            _ => {
                if is_utf8_character_begin {
                    utf16_offset += 1 + i32::from(c >= 0xf0); // >= 4 bytes in symbol => surrogate pair
                }
                let mut skipped = false;
                if c == 0xe2 && pos + 2 < text_size {
                    let mut next = bytes[pos + 1];
                    if next == 0x80 {
                        next = bytes[pos + 2];
                        if (0xa8..=0xae).contains(&next) {
                            pos += 2;
                            utf16_skipped += 1;
                            skipped = true;
                        }
                    }
                }
                if !skipped && c == 0xcc && pos + 1 < text_size {
                    let next = bytes[pos + 1];
                    // remove vertical lines
                    if next == 0xb3 || next == 0xbf || next == 0x8a {
                        pos += 1;
                        utf16_skipped += 1;
                        skipped = true;
                    }
                }

                if !skipped {
                    result.push(bytes[pos]);
                }
            }
        }

        pos += 1;
    }

    if current_entity != entities.len() {
        return Err(Status::error(
            400,
            &format!(
                "Entity begins after the end of the text at UTF-16 offset {}",
                entities[current_entity].offset
            ),
        ));
    }
    if let Some(top) = nested_entities_stack.last() {
        let entity = &entities[top.index];
        return Err(Status::error(
            400,
            &format!(
                "Entity beginning at UTF-16 offset {} ends after the end of the text at UTF-16 offset {}",
                entity.offset,
                entity.offset + entity.length
            ),
        ));
    }

    crate::misc::replace_offending_characters(&mut result);
    // Only whole characters were replaced or removed, so the bytes stay
    // valid UTF-8.
    Ok(String::from_utf8(result).expect("clean_input_string_with_entities preserves UTF-8"))
}

/// `remove_invalid_entities` (MessageEntity.cpp:4270-4313): removes empty
/// entities; they must be sorted by offset and length, but not necessarily
/// by type. Returns `(last_non_whitespace_pos, last_non_whitespace_utf16_offset)`.
fn remove_invalid_entities(text: &str, entities: &mut Vec<MessageEntity>) -> (usize, i32) {
    if entities.is_empty() {
        // fast path
        let bytes = text.as_bytes();
        for pos in 0..bytes.len() {
            let back_pos = bytes.len() - pos - 1;
            let c = bytes[back_pos];
            if c != b'\n' && c != b' ' {
                return (back_pos, 0 /*unused*/);
            }
        }

        return (text.len(), -1);
    }

    // check_is_sorted(entities);

    let mut last_non_whitespace_pos = text.len();
    let bytes = text.as_bytes();

    let mut utf16_offset = 0i32;
    let mut last_non_whitespace_utf16_offset = -1i32;

    remove_empty_entities(entities);

    let mut pos = 0usize;
    while pos < bytes.len() {
        let c = bytes[pos];
        match c {
            b'\n' | 32 => {}
            _ => {
                // The C++ reads text[pos + 1] past the end (the NUL
                // terminator, itself a first code unit — the loop exits)
                // and advances the loop `pos` over the whole symbol; the
                // continuation bytes must not be counted separately.
                while pos + 1 < bytes.len()
                    && !tdutils::utf8::is_utf8_character_first_code_unit(bytes[pos + 1])
                {
                    pos += 1;
                }
                utf16_offset += i32::from(c >= 0xf0); // >= 4 bytes in symbol => surrogate pair
                last_non_whitespace_pos = pos;
                last_non_whitespace_utf16_offset = utf16_offset;
            }
        }

        utf16_offset += 1;
        pos += 1;
    }
    (last_non_whitespace_pos, last_non_whitespace_utf16_offset)
}

/// `fix_formatted_text` (MessageEntity.cpp:4477-4604).
#[allow(clippy::too_many_arguments)]
pub fn fix_formatted_text(
    text: &mut String,
    entities: &mut Vec<MessageEntity>,
    allow_empty: bool,
    allow_empty_string: bool,
    skip_new_entities: bool,
    skip_bot_commands: bool,
    skip_media_timestamps: bool,
    skip_trim: bool,
    mut ltrim_count: Option<&mut i32>,
) -> Result<(), Status> {
    let mut result: String;
    if entities.is_empty() {
        // fast path
        let Some(cleaned) = crate::misc::clean_input_string(text) else {
            return Err(Status::error(400, "Strings must be encoded in UTF-8"));
        };
        result = cleaned;
    } else {
        if !tdutils::utf8::check_utf8(text.as_bytes()) {
            return Err(Status::error(400, "Strings must be encoded in UTF-8"));
        }

        for entity in entities.iter() {
            if entity.offset < 0 || entity.offset > 1000000 {
                return Err(Status::error(
                    400,
                    &format!("Receive an entity with incorrect offset {}", entity.offset),
                ));
            }
            if entity.length < 0 || entity.length > 1000000 {
                return Err(Status::error(
                    400,
                    &format!("Receive an entity with incorrect length {}", entity.length),
                ));
            }
        }
        remove_empty_entities(entities);

        fix_entities(entities);

        result = clean_input_string_with_entities(text, entities)?;
    }

    // now entities are still sorted by offset and length, but not type,
    // because some characters could be deleted and after that some entities begin to share a common end

    let (last_non_whitespace_pos, last_non_whitespace_utf16_offset) =
        remove_invalid_entities(&result, entities);
    if last_non_whitespace_utf16_offset == -1 {
        if allow_empty {
            text.clear();
            entities.clear();
            return Ok(());
        }
        return Err(Status::error(400, "Text must be non-empty"));
    }

    // re-fix entities if needed after removal of some characters
    // the sort order can be incorrect by type
    // some splittable entities may be needed to be concatenated
    fix_entities(entities);

    if let Some(ltrim_count) = ltrim_count.as_mut() {
        **ltrim_count = 0;
    }
    if skip_trim {
        *text = result;
    } else {
        // rtrim
        assert!(
            last_non_whitespace_pos < result.len(),
            "the C++ CHECK(last_non_whitespace_pos < result.size())"
        );
        result.truncate(last_non_whitespace_pos + 1);
        while entities
            .last()
            .is_some_and(|entity| entity.offset > last_non_whitespace_utf16_offset)
        {
            entities.pop();
        }
        let mut need_sort = false;
        for entity in entities.iter_mut() {
            if entity.offset + entity.length > last_non_whitespace_utf16_offset + 1 {
                entity.length = last_non_whitespace_utf16_offset + 1 - entity.offset;
                need_sort = true;
                assert!(entity.length > 0, "the C++ CHECK(entity.length > 0)");
            }
        }
        if need_sort {
            sort_entities(entities);
        }

        // ltrim
        let mut first_non_whitespaces_pos = 0usize;
        let first_entity_begin_pos = if entities.is_empty() {
            result.len()
        } else {
            entities[0].offset as usize
        };
        let result_bytes = result.as_bytes();
        while first_non_whitespaces_pos < first_entity_begin_pos
            && (result_bytes[first_non_whitespaces_pos] == b' '
                || result_bytes[first_non_whitespaces_pos] == b'\n')
        {
            first_non_whitespaces_pos += 1;
        }
        if first_non_whitespaces_pos > 0 {
            let offset = first_non_whitespaces_pos as i32;
            if let Some(ltrim_count) = ltrim_count {
                *ltrim_count = offset;
            }
            *text = result[first_non_whitespaces_pos..].to_owned();
            for entity in entities.iter_mut() {
                entity.offset -= offset;
                assert!(entity.offset >= 0, "the C++ CHECK(entity.offset >= 0)");
            }
        } else {
            *text = result;
        }
    }
    debug_assert!(
        tdutils::utf8::check_utf8(text.as_bytes()),
        "the C++ LOG_CHECK(check_utf8(text))"
    );

    if crate::misc::is_empty_string(text) {
        if !allow_empty {
            return Err(Status::error(400, "Text must be non-empty"));
        }
        if !allow_empty_string {
            text.clear();
            entities.clear();
        }
    }

    const LENGTH_LIMIT: usize = 35000; // server-side limit
    if text.len() > LENGTH_LIMIT {
        let mut new_size = LENGTH_LIMIT;
        while !tdutils::utf8::is_utf8_character_first_code_unit(text.as_bytes()[new_size]) {
            new_size -= 1;
        }
        text.truncate(new_size);

        let text_utf16_length = text_length(text);
        entities.retain(|entity| entity.offset + entity.length <= text_utf16_length);
    }

    if !skip_new_entities {
        let found = find_entities(text, skip_bot_commands, skip_media_timestamps);
        merge_new_entities(entities, found);
    } else if !skip_media_timestamps {
        let found = find_media_timestamp_entities(text);
        merge_new_entities(entities, found);
    }

    Ok(())
}

/// `get_message_text` (MessageEntity.cpp:4606-4628).
#[allow(clippy::too_many_arguments)] // the C++ signature has the same arity
pub fn get_message_text<U: UserManagerLike + ?Sized>(
    user_manager: Option<&mut U>,
    mut message_text: String,
    server_entities: Vec<tdmtproto::telegram_api::MessageEntity>,
    skip_new_entities: bool,
    skip_media_timestamps: bool,
    send_date: i32,
    from_album: bool,
    source: &str,
) -> FormattedText {
    let mut entities = get_server_message_entities(user_manager, server_entities, source);
    let status = fix_formatted_text(
        &mut message_text,
        &mut entities,
        true,
        false,
        skip_new_entities,
        true,
        skip_media_timestamps,
        false,
        None,
    );
    if let Err(status) = status {
        // message entities in media albums can be wrong because of a long time ago fixed server-side bug
        if !from_album && (send_date == 0 || send_date > 1600340000) {
            // approximate fix date
            tdutils::log_error!(
                "Receive error {} while parsing message text from {} sent at {}",
                status.message(),
                source,
                send_date
            );
        }
        if let Some(cleaned) = crate::misc::clean_input_string(&message_text) {
            message_text = cleaned;
        } else {
            message_text.clear();
        }
        entities = find_entities(&message_text, false, skip_media_timestamps);
    }
    FormattedText {
        text: message_text,
        entities,
    }
}

/// `truncate_formatted_text` (MessageEntity.cpp:4630-4647).
pub fn truncate_formatted_text(text: &mut FormattedText, length: usize) {
    let result_size = tdutils::utf8::utf8_truncate(&text.text, length).len();
    if result_size == text.text.len() {
        return;
    }
    text.text.truncate(result_size);
    let utf16_length = text_length(&text.text);
    for entity in text.entities.iter_mut() {
        if entity.offset + entity.length > utf16_length {
            if entity.offset >= utf16_length || is_continuous_entity(entity_type(entity)) {
                entity.length = 0;
                continue;
            }
            entity.length = utf16_length - entity.offset; // truncate the entity
        }
    }
    remove_empty_entities(&mut text.entities);
}

/// `get_formatted_text(user_manager, string&&, server_entities, ...)`
/// (MessageEntity.cpp:4096-4110).
pub fn get_formatted_text<U: UserManagerLike + ?Sized>(
    user_manager: Option<&mut U>,
    mut text: String,
    server_entities: Vec<tdmtproto::telegram_api::MessageEntity>,
    skip_media_timestamps: bool,
    skip_trim: bool,
    source: &str,
) -> FormattedText {
    let mut entities = get_server_message_entities(user_manager, server_entities, source);
    let status = fix_formatted_text(
        &mut text,
        &mut entities,
        true,
        false,
        true,
        true,
        skip_media_timestamps,
        skip_trim,
        None,
    );
    if let Err(status) = status {
        tdutils::log_error!(
            "Receive error {} from {} while parsing message text",
            status.message(),
            source
        );
        if let Some(cleaned) = crate::misc::clean_input_string(&text) {
            text = cleaned;
        } else {
            text.clear();
        }
        entities = find_entities(&text, true, skip_media_timestamps);
    }
    FormattedText { text, entities }
}

/// `get_formatted_text(user_manager, textWithEntities, skip_media_timestamps,
/// skip_trim, source)` (MessageEntity.cpp:4112-4120).
pub fn get_formatted_text_of_text_with_entities<U: UserManagerLike + ?Sized>(
    user_manager: Option<&mut U>,
    text_with_entities: Option<tdmtproto::telegram_api::TextWithEntities>,
    skip_media_timestamps: bool,
    skip_trim: bool,
    source: &str,
) -> FormattedText {
    let Some(text_with_entities) = text_with_entities else {
        return FormattedText::default();
    };
    get_formatted_text(
        user_manager,
        text_with_entities.text,
        text_with_entities.entities,
        skip_media_timestamps,
        skip_trim,
        source,
    )
}

/// `get_formatted_text(td, dialog_id, td_api::formattedText, is_bot,
/// allow_empty, skip_media_timestamps, skip_trim, ltrim_count)`
/// (MessageEntity.cpp:4649-4678). The Td-owned inputs are threaded
/// explicitly: the OptionManager flags (`always_parse_markdown` and
/// `session_count`) and the `remove_unallowed_entities` capabilities.
#[allow(clippy::too_many_arguments)]
pub fn get_formatted_text_of_input<U: UserManagerLike + ?Sized>(
    mut user_manager: Option<&mut U>,
    dialog_id: DialogId,
    text: Option<td_api::FormattedText>,
    is_bot: bool,
    allow_empty: bool,
    skip_media_timestamps: bool,
    skip_trim: bool,
    ltrim_count: Option<&mut i32>,
    always_parse_markdown: bool,
    session_count: i32,
    is_premium_custom_emoji: IsPremiumCustomEmoji<'_>,
    can_use_premium_custom_emoji_in_dialog: bool,
) -> Result<FormattedText, Status> {
    let Some(mut text) = text else {
        if allow_empty {
            return Ok(FormattedText::default());
        }
        return Err(Status::error(400, "Text must be non-empty"));
    };

    let mut entities = get_message_entities(user_manager.as_deref_mut(), text.entities, true)?;
    let need_skip_bot_commands =
        need_always_skip_bot_commands(user_manager.as_deref(), dialog_id, is_bot);
    let skip_new_entities = is_bot && session_count > 1;
    fix_formatted_text(
        &mut text.text,
        &mut entities,
        allow_empty,
        allow_empty && is_bot,
        skip_new_entities || always_parse_markdown,
        skip_new_entities || need_skip_bot_commands,
        is_bot || skip_media_timestamps || always_parse_markdown,
        skip_trim,
        ltrim_count,
    )?;

    let mut result = FormattedText {
        text: text.text,
        entities,
    };
    if always_parse_markdown {
        result = parse_markdown_v3(result);
        fix_formatted_text(
            &mut result.text,
            &mut result.entities,
            allow_empty,
            allow_empty && is_bot,
            false,
            need_skip_bot_commands,
            is_bot || skip_media_timestamps,
            skip_trim,
            None,
        )
        .expect("the C++ .ensure()");
    }
    remove_unallowed_entities(
        &mut result,
        dialog_id,
        user_manager.as_deref(),
        is_premium_custom_emoji,
        can_use_premium_custom_emoji_in_dialog,
    );
    Ok(result)
}

/// `get_input_text_with_entities` (MessageEntity.cpp:4089-4094).
pub fn get_input_text_with_entities<U: UserManagerLike + ?Sized>(
    user_manager: Option<&mut U>,
    text: &FormattedText,
    source: &str,
) -> tdmtproto::telegram_api::TextWithEntities {
    tdmtproto::telegram_api::TextWithEntities {
        text: text.text.clone(),
        entities: get_input_message_entities(user_manager, &text.entities, source),
    }
}

/// `get_input_message_entities(user_manager, entities, source)`
/// (MessageEntity.cpp:4742-4829).
pub fn get_input_message_entities<U: UserManagerLike + ?Sized>(
    mut user_manager: Option<&mut U>,
    entities: &[MessageEntity],
    _source: &str,
) -> Vec<tdmtproto::telegram_api::MessageEntity> {
    use tdmtproto::telegram_api;

    let mut result: Vec<telegram_api::MessageEntity> = Vec::new();
    let mut splittable_entities: Vec<MessageEntity> = Vec::new();
    const MAX_USER_ENTITY_COUNT: usize = 100; // server-side limit
    let mut user_entity_count = 0usize;
    for entity in entities {
        let r#type = entity_type(entity);
        if !is_user_entity(r#type) {
            continue;
        }
        if is_splittable_entity(r#type) {
            splittable_entities.push(entity.clone());
            continue;
        }
        if r#type == Type::CustomEmoji {
            result.push(telegram_api::MessageEntity::messageEntityCustomEmoji(
                telegram_api::messageEntityCustomEmoji {
                    offset: entity.offset,
                    length: entity.length,
                    document_id: entity.custom_emoji_id.get(),
                },
            ));
            continue;
        }
        if user_entity_count >= MAX_USER_ENTITY_COUNT {
            continue;
        }
        user_entity_count += 1;
        match r#type {
            Type::BlockQuote => {
                result.push(telegram_api::MessageEntity::messageEntityBlockquote(
                    telegram_api::messageEntityBlockquote {
                        flags: 0,
                        collapsed: false,
                        offset: entity.offset,
                        length: entity.length,
                    },
                ));
            }
            Type::Code => {
                result.push(telegram_api::MessageEntity::messageEntityCode(
                    telegram_api::messageEntityCode {
                        offset: entity.offset,
                        length: entity.length,
                    },
                ));
            }
            Type::Pre => {
                result.push(telegram_api::MessageEntity::messageEntityPre(
                    telegram_api::messageEntityPre {
                        offset: entity.offset,
                        length: entity.length,
                        language: String::new(),
                    },
                ));
            }
            Type::PreCode => {
                result.push(telegram_api::MessageEntity::messageEntityPre(
                    telegram_api::messageEntityPre {
                        offset: entity.offset,
                        length: entity.length,
                        language: entity.argument.clone(),
                    },
                ));
            }
            Type::TextUrl => {
                result.push(telegram_api::MessageEntity::messageEntityTextUrl(
                    telegram_api::messageEntityTextUrl {
                        offset: entity.offset,
                        length: entity.length,
                        url: entity.argument.clone(),
                    },
                ));
            }
            Type::MentionName => {
                assert!(
                    user_manager.is_some(),
                    "the C++ CHECK(user_manager != nullptr)"
                );
                let input_user = user_manager
                    .as_deref_mut()
                    .unwrap()
                    .get_input_user_force(entity.user_id);
                result.push(telegram_api::MessageEntity::inputMessageEntityMentionName(
                    telegram_api::inputMessageEntityMentionName {
                        offset: entity.offset,
                        length: entity.length,
                        user_id: Some(input_user),
                    },
                ));
            }
            Type::ExpandableBlockQuote => {
                result.push(telegram_api::MessageEntity::messageEntityBlockquote(
                    telegram_api::messageEntityBlockquote {
                        flags: 0,
                        collapsed: true,
                        offset: entity.offset,
                        length: entity.length,
                    },
                ));
            }
            Type::FormattedDate => {
                result.push(telegram_api::MessageEntity::messageEntityFormattedDate(
                    entity
                        .date
                        .get_input_message_entity_formatted_date(entity.offset, entity.length),
                ))
            }
            _ => unreachable!("the C++ UNREACHABLE()"),
        }
    }
    split_entities(&mut splittable_entities, &[]);
    for entity in &splittable_entities {
        if user_entity_count >= MAX_USER_ENTITY_COUNT {
            break;
        }
        user_entity_count += 1;

        let r#type = entity_type(entity);
        match r#type {
            Type::Bold => {
                result.push(telegram_api::MessageEntity::messageEntityBold(
                    telegram_api::messageEntityBold {
                        offset: entity.offset,
                        length: entity.length,
                    },
                ));
            }
            Type::Italic => {
                result.push(telegram_api::MessageEntity::messageEntityItalic(
                    telegram_api::messageEntityItalic {
                        offset: entity.offset,
                        length: entity.length,
                    },
                ));
            }
            Type::Underline => {
                result.push(telegram_api::MessageEntity::messageEntityUnderline(
                    telegram_api::messageEntityUnderline {
                        offset: entity.offset,
                        length: entity.length,
                    },
                ));
            }
            Type::Strikethrough => {
                result.push(telegram_api::MessageEntity::messageEntityStrike(
                    telegram_api::messageEntityStrike {
                        offset: entity.offset,
                        length: entity.length,
                    },
                ));
            }
            Type::Spoiler => {
                result.push(telegram_api::MessageEntity::messageEntitySpoiler(
                    telegram_api::messageEntitySpoiler {
                        offset: entity.offset,
                        length: entity.length,
                    },
                ));
            }
            _ => unreachable!("the C++ UNREACHABLE()"),
        }
    }

    result
}

/// `get_input_message_entities(user_manager, text, source)`
/// (MessageEntity.cpp:4831-4838).
pub fn get_input_message_entities_of_text<U: UserManagerLike + ?Sized>(
    user_manager: Option<&mut U>,
    text: Option<&FormattedText>,
    source: &str,
) -> Vec<tdmtproto::telegram_api::MessageEntity> {
    if let Some(text) = text
        && !text.entities.is_empty()
    {
        return get_input_message_entities(user_manager, &text.entities, source);
    }
    Vec::new()
}

/// `SecretChatLayer` (SecretChatLayer.h:11-19) — only the entity-gating
/// values are carried.
const SECRET_CHAT_LAYER_NEW_ENTITIES: i32 = 101;
const SECRET_CHAT_LAYER_SPOILER_AND_CUSTOM_EMOJI_ENTITIES: i32 = 144;

/// `get_input_secret_message_entities` (MessageEntity.cpp:3579-3667).
pub fn get_input_secret_message_entities(
    entities: &[MessageEntity],
    layer: i32,
) -> Vec<tdmtproto::secret_api::MessageEntity> {
    use tdmtproto::secret_api;

    let mut result: Vec<secret_api::MessageEntity> = Vec::new();
    for entity in entities {
        let r#type = entity_type(entity);
        match r#type {
            Type::Mention => result.push(secret_api::MessageEntity::messageEntityMention(
                secret_api::messageEntityMention {
                    offset: entity.offset,
                    length: entity.length,
                },
            )),
            Type::Hashtag => result.push(secret_api::MessageEntity::messageEntityHashtag(
                secret_api::messageEntityHashtag {
                    offset: entity.offset,
                    length: entity.length,
                },
            )),
            Type::Cashtag => {}
            Type::BotCommand => {}
            Type::PhoneNumber => {}
            Type::BankCardNumber => {}
            Type::Url => result.push(secret_api::MessageEntity::messageEntityUrl(
                secret_api::messageEntityUrl {
                    offset: entity.offset,
                    length: entity.length,
                },
            )),
            Type::EmailAddress => result.push(secret_api::MessageEntity::messageEntityEmail(
                secret_api::messageEntityEmail {
                    offset: entity.offset,
                    length: entity.length,
                },
            )),
            Type::Bold => result.push(secret_api::MessageEntity::messageEntityBold(
                secret_api::messageEntityBold {
                    offset: entity.offset,
                    length: entity.length,
                },
            )),
            Type::Italic => result.push(secret_api::MessageEntity::messageEntityItalic(
                secret_api::messageEntityItalic {
                    offset: entity.offset,
                    length: entity.length,
                },
            )),
            Type::Underline => {
                if layer >= SECRET_CHAT_LAYER_NEW_ENTITIES {
                    result.push(secret_api::MessageEntity::messageEntityUnderline(
                        secret_api::messageEntityUnderline {
                            offset: entity.offset,
                            length: entity.length,
                        },
                    ));
                }
            }
            Type::Strikethrough => {
                if layer >= SECRET_CHAT_LAYER_NEW_ENTITIES {
                    result.push(secret_api::MessageEntity::messageEntityStrike(
                        secret_api::messageEntityStrike {
                            offset: entity.offset,
                            length: entity.length,
                        },
                    ));
                }
            }
            // The C++ blockquote pushes are commented out upstream.
            Type::BlockQuote => {}
            Type::ExpandableBlockQuote => {}
            Type::Code => result.push(secret_api::MessageEntity::messageEntityCode(
                secret_api::messageEntityCode {
                    offset: entity.offset,
                    length: entity.length,
                },
            )),
            Type::Pre => result.push(secret_api::MessageEntity::messageEntityPre(
                secret_api::messageEntityPre {
                    offset: entity.offset,
                    length: entity.length,
                    language: Vec::new(),
                },
            )),
            Type::PreCode => result.push(secret_api::MessageEntity::messageEntityPre(
                secret_api::messageEntityPre {
                    offset: entity.offset,
                    length: entity.length,
                    language: entity.argument.clone().into_bytes(),
                },
            )),
            Type::TextUrl => result.push(secret_api::MessageEntity::messageEntityTextUrl(
                secret_api::messageEntityTextUrl {
                    offset: entity.offset,
                    length: entity.length,
                    url: entity.argument.clone().into_bytes(),
                },
            )),
            Type::MentionName => {}
            Type::MediaTimestamp => {}
            Type::Spoiler => {
                if layer >= SECRET_CHAT_LAYER_SPOILER_AND_CUSTOM_EMOJI_ENTITIES {
                    result.push(secret_api::MessageEntity::messageEntitySpoiler(
                        secret_api::messageEntitySpoiler {
                            offset: entity.offset,
                            length: entity.length,
                        },
                    ));
                }
            }
            Type::CustomEmoji => {
                if layer >= SECRET_CHAT_LAYER_SPOILER_AND_CUSTOM_EMOJI_ENTITIES {
                    result.push(secret_api::MessageEntity::messageEntityCustomEmoji(
                        secret_api::messageEntityCustomEmoji {
                            offset: entity.offset,
                            length: entity.length,
                            document_id: entity.custom_emoji_id.get(),
                        },
                    ));
                }
            }
            Type::FormattedDate => {}
        }
    }

    result
}

/// `has_media_timestamps` (MessageEntity.cpp:4689-4700).
pub fn has_media_timestamps(
    text: Option<&FormattedText>,
    min_media_timestamp: i32,
    max_media_timestamp: i32,
) -> bool {
    let Some(text) = text else {
        return false;
    };
    for entity in &text.entities {
        if entity.r#type == Some(Type::MediaTimestamp)
            && min_media_timestamp <= entity.media_timestamp
            && entity.media_timestamp <= max_media_timestamp
        {
            return true;
        }
    }
    false
}

/// `has_bot_commands` (MessageEntity.cpp:4702-4712).
pub fn has_bot_commands(text: Option<&FormattedText>) -> bool {
    let Some(text) = text else {
        return false;
    };
    for entity in &text.entities {
        if entity.r#type == Some(Type::BotCommand) {
            return true;
        }
    }
    false
}

/// `add_formatted_text_dependencies` (MessageEntity.cpp:4680-4687). The
/// `Dependencies` type lands with the messages wave; the port threads its
/// user-id `add` (which filters invalid ids, Dependencies.cpp:22-27) as a
/// closure so the entity walk is complete now.
pub fn add_formatted_text_dependencies(
    dependencies: &mut dyn FnMut(UserId),
    text: Option<&FormattedText>,
) {
    let Some(text) = text else {
        return;
    };
    for entity in &text.entities {
        if entity.user_id.is_valid() {
            dependencies(entity.user_id);
        }
    }
}

/// `need_always_skip_bot_commands` (MessageEntity.cpp:4714-4740).
pub fn need_always_skip_bot_commands<U: UserManagerLike + ?Sized>(
    user_manager: Option<&U>,
    dialog_id: DialogId,
    is_bot: bool,
) -> bool {
    if !dialog_id.is_valid() {
        return true;
    }
    if is_bot {
        return false;
    }

    match dialog_id {
        DialogId::User(user_id) => {
            user_id == U::get_replies_bot_user_id()
                || user_id == U::get_verification_codes_bot_user_id()
                || user_manager.is_none_or(|user_manager| !user_manager.is_user_bot(user_id))
        }
        DialogId::SecretChat(secret_chat_id) => {
            let user_id = user_manager.map_or(UserId::default(), |user_manager| {
                user_manager.get_secret_chat_user_id(secret_chat_id)
            });
            !user_id.is_valid()
                || user_manager.is_none_or(|user_manager| !user_manager.is_user_bot(user_id))
        }
        DialogId::Chat(_) | DialogId::Channel(_) | DialogId::None => false,
    }
}

// ===== M.8.6 — Markdown v1 (MessageEntity.cpp:1929-2046) =====

/// `td::is_space` (misc.h:107-109).
fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\r' | b'\n' | 0 | 0x0b)
}

/// `parse_markdown` (MessageEntity.cpp:1929-2046): the legacy `__bold__`,
/// `*bold*`, `` `code` ``, `` ```pre``` `` and `[text](url)` dialect. The
/// C++ compacts `text` in place; the port rewrites `text` at the end.
pub fn parse_markdown(text: &mut String) -> Result<Vec<MessageEntity>, Status> {
    let bytes = text.as_bytes();
    let size = bytes.len();
    // The C++ reads past the end of the NUL-terminated string; those reads
    // yield '\0'.
    let at = |i: usize| -> u8 { bytes.get(i).copied().unwrap_or(0) };

    let mut result: Vec<u8> = Vec::with_capacity(size);
    let mut entities: Vec<MessageEntity> = Vec::new();
    let mut utf16_offset = 0i32;
    let mut i = 0usize;
    while i < size {
        let c = at(i);
        if c == b'\\' && matches!(at(i + 1), b'_' | b'*' | b'`' | b'[') {
            i += 1;
            result.push(at(i));
            utf16_offset += 1;
            i += 1;
            continue;
        }
        if c != b'_' && c != b'*' && c != b'`' && c != b'[' {
            if tdutils::utf8::is_utf8_character_first_code_unit(c) {
                utf16_offset += 1 + i32::from(c >= 0xf0); // >= 4 bytes in symbol => surrogate pair
            }
            result.push(at(i));
            i += 1;
            continue;
        }

        // we are at begin of the entity
        let begin_pos = i;
        let mut end_character = at(i);
        let mut is_pre = false;
        if c == b'[' {
            end_character = b']';
        }

        i += 1;

        let mut language = String::new();
        if c == b'`' && at(i) == b'`' && at(i + 1) == b'`' {
            i += 2;
            is_pre = true;
            let mut language_end = i;
            while !is_space(at(language_end)) && at(language_end) != b'`' {
                language_end += 1;
            }
            if i != language_end && language_end < size && at(language_end) != b'`' {
                language.push_str(&text[i..language_end]);
                i = language_end;
            }
            // skip one new line in the beginning of the text
            if at(i) == b'\n' || at(i) == b'\r' {
                if (at(i + 1) == b'\n' || at(i + 1) == b'\r') && at(i) != at(i + 1) {
                    i += 2;
                } else {
                    i += 1;
                }
            }
        }

        let entity_offset = utf16_offset;
        while i < size
            && (at(i) != end_character || (is_pre && !(at(i + 1) == b'`' && at(i + 2) == b'`')))
        {
            let cur_ch = at(i);
            if tdutils::utf8::is_utf8_character_first_code_unit(cur_ch) {
                utf16_offset += 1 + i32::from(cur_ch >= 0xf0); // >= 4 bytes in symbol => surrogate pair
            }
            result.push(cur_ch);
            i += 1;
        }
        if i == size {
            return Err(Status::error(
                400,
                &format!(
                    "Can't find end of the entity starting at byte offset {}",
                    begin_pos
                ),
            ));
        }

        if entity_offset != utf16_offset {
            let entity_length = utf16_offset - entity_offset;
            match c {
                b'_' => entities.push(MessageEntity::new(
                    Type::Italic,
                    entity_offset,
                    entity_length,
                )),
                b'*' => entities.push(MessageEntity::new(Type::Bold, entity_offset, entity_length)),
                b'[' => {
                    let url = if at(i + 1) != b'(' {
                        // use text as a URL
                        text[begin_pos + 1..i].to_owned()
                    } else {
                        let mut url_bytes = Vec::new();
                        i += 2;
                        while i < size && at(i) != b')' {
                            url_bytes.push(at(i));
                            i += 1;
                        }
                        // Whole characters only, so the bytes are valid UTF-8.
                        String::from_utf8(url_bytes).expect("a URL is built from whole characters")
                    };
                    let user_id = crate::link_manager::get_link_user_id(&url);
                    if user_id.is_valid() {
                        entities.push(MessageEntity::mention_name(
                            entity_offset,
                            entity_length,
                            user_id,
                        ));
                    } else {
                        let url = crate::link_manager::get_checked_link(&url);
                        if !url.is_empty() {
                            entities.push(MessageEntity::with_argument(
                                Type::TextUrl,
                                entity_offset,
                                entity_length,
                                url,
                            ));
                        }
                    }
                }
                b'`' => {
                    if is_pre {
                        if language.is_empty() {
                            entities.push(MessageEntity::new(
                                Type::Pre,
                                entity_offset,
                                entity_length,
                            ));
                        } else {
                            entities.push(MessageEntity::with_argument(
                                Type::PreCode,
                                entity_offset,
                                entity_length,
                                language,
                            ));
                        }
                    } else {
                        entities.push(MessageEntity::new(Type::Code, entity_offset, entity_length));
                    }
                }
                _ => unreachable!("the C++ UNREACHABLE()"),
            }
        }
        if is_pre {
            i += 2;
        }
        // The C++ for-loop's increment: move past the entity terminator.
        i += 1;
    }
    *text = String::from_utf8(result).expect("parse_markdown preserves UTF-8");
    Ok(entities)
}

// ===== M.8.7 — Markdown v2 (MessageEntity.cpp:2048-2400) =====

/// The per-entity state of the v2 parser (MessageEntity.cpp:2057-2073).
struct EntityInfoV2 {
    r#type: Type,
    argument: String,
    entity_offset: i32,
    entity_byte_offset: usize,
    entity_begin_pos: usize,
}

/// `parse_markdown_v2` (MessageEntity.cpp:2048-2363): the MarkdownV2
/// dialect with nested entities, expandable blockquotes and custom
/// emoji/`FormattedDate` links. The C++ compacts `text` in place; the port
/// rewrites `text` at the end. The tail keeps the C++'s (unread) final
/// `have_blockquote` clear.
#[allow(unused_assignments)]
pub fn parse_markdown_v2(text: &mut String) -> Result<Vec<MessageEntity>, Status> {
    let bytes = text.as_bytes();
    // The C++ reads past the end of the NUL-terminated string; those reads
    // yield '\0'.
    let at = |i: usize| -> u8 { bytes.get(i).copied().unwrap_or(0) };

    let mut result: Vec<u8> = Vec::with_capacity(text.len());
    let mut entities: Vec<MessageEntity> = Vec::new();
    let mut utf16_offset = 0i32;
    let mut nested_entities: Vec<EntityInfoV2> = Vec::new();

    let mut have_blockquote = false;
    let mut can_start_blockquote = true;
    let mut i = 0usize;
    while i < text.len() {
        let c = at(i);
        if c == b'\\' && at(i + 1) > 0 && at(i + 1) <= 126 {
            i += 1;
            utf16_offset += 1;
            result.push(at(i));
            if at(i) != b'\r' {
                can_start_blockquote = at(i) == b'\n';
            }
            i += 1;
            continue;
        }

        // MessageEntity.cpp:2085: "_*[]()~`>#+-=|{}.!\n".
        let mut reserved_characters: &[u8] = b"_*[]()~`>#+-=|{}.!\n";
        if let Some(back) = nested_entities.last() {
            match back.r#type {
                Type::Code | Type::Pre | Type::PreCode => reserved_characters = b"`",
                _ => {}
            }
        }

        if !reserved_characters.contains(&c) {
            if tdutils::utf8::is_utf8_character_first_code_unit(c) {
                utf16_offset += 1 + i32::from(c >= 0xf0); // >= 4 bytes in symbol => surrogate pair
                if c != b'\r' {
                    can_start_blockquote = false;
                }
            }
            result.push(c);
            i += 1;
            continue;
        }

        let is_end_of_an_entity = 'is_end: {
            if nested_entities.is_empty() {
                break 'is_end false;
            }
            if have_blockquote && c == b'\n' && (i + 1 == text.len() || at(i + 1) != b'>') {
                break 'is_end true;
            }
            match nested_entities.last().unwrap().r#type {
                Type::Bold => c == b'*',
                Type::Italic => c == b'_' && at(i + 1) != b'_',
                Type::Code => c == b'`',
                Type::Pre | Type::PreCode => c == b'`' && at(i + 1) == b'`' && at(i + 2) == b'`',
                Type::TextUrl => c == b']',
                Type::Underline => c == b'_' && at(i + 1) == b'_',
                Type::Strikethrough => c == b'~',
                Type::Spoiler => c == b'|' && at(i + 1) == b'|',
                Type::CustomEmoji => c == b']',
                Type::BlockQuote => false,
                _ => unreachable!("the C++ UNREACHABLE()"),
            }
        };
        if !is_end_of_an_entity {
            // begin of an entity
            let mut r#type;
            let mut argument = String::new();
            let entity_byte_offset = i;
            match c {
                b'_' => {
                    if at(i + 1) == b'_' {
                        r#type = Type::Underline;
                        i += 1;
                    } else {
                        r#type = Type::Italic;
                    }
                }
                b'*' => r#type = Type::Bold,
                b'~' => r#type = Type::Strikethrough,
                b'|' if at(i + 1) == b'|' => {
                    i += 1;
                    r#type = Type::Spoiler;
                }
                b'|' => {
                    return Err(Status::error(
                        400,
                        &format!(
                            "Character '{}' is reserved and must be escaped with the preceding '\\'",
                            at(i) as char
                        ),
                    ));
                }
                b'[' => r#type = Type::TextUrl,
                b'`' => {
                    if at(i + 1) == b'`' && at(i + 2) == b'`' {
                        i += 3;
                        r#type = Type::Pre;
                        let mut language_end = i;
                        while !is_space(at(language_end)) && at(language_end) != b'`' {
                            language_end += 1;
                        }
                        if i != language_end
                            && language_end < text.len()
                            && at(language_end) != b'`'
                        {
                            r#type = Type::PreCode;
                            argument = text[i..language_end].to_owned();
                            i = language_end;
                        }
                        // skip one new line in the beginning of the text
                        if at(i) == b'\n' || at(i) == b'\r' {
                            if (at(i + 1) == b'\n' || at(i + 1) == b'\r') && at(i) != at(i + 1) {
                                i += 2;
                            } else {
                                i += 1;
                            }
                        }

                        i -= 1;
                    } else {
                        r#type = Type::Code;
                    }
                }
                b'!' if at(i + 1) == b'[' => {
                    i += 1;
                    r#type = Type::CustomEmoji;
                }
                b'!' => {
                    return Err(Status::error(
                        400,
                        &format!(
                            "Character '{}' is reserved and must be escaped with the preceding '\\'",
                            at(i) as char
                        ),
                    ));
                }
                b'\n' => {
                    utf16_offset += 1;
                    result.push(b'\n');
                    can_start_blockquote = true;
                    // the C++ `Type::Size` sentinel — no entity is pushed
                    i += 1;
                    continue;
                }
                b'>' if can_start_blockquote => {
                    if have_blockquote {
                        // the C++ `Type::Size` sentinel — no entity is pushed
                        i += 1;
                        continue;
                    }
                    r#type = Type::BlockQuote;
                    have_blockquote = true;
                }
                b'>' => {
                    return Err(Status::error(
                        400,
                        &format!(
                            "Character '{}' is reserved and must be escaped with the preceding '\\'",
                            at(i) as char
                        ),
                    ));
                }
                _ => {
                    return Err(Status::error(
                        400,
                        &format!(
                            "Character '{}' is reserved and must be escaped with the preceding '\\'",
                            at(i) as char
                        ),
                    ));
                }
            }
            nested_entities.push(EntityInfoV2 {
                r#type,
                argument,
                entity_offset: utf16_offset,
                entity_byte_offset,
                entity_begin_pos: result.len(),
            });
        } else {
            // end of an entity
            let mut r#type = nested_entities.last().unwrap().r#type;
            if c == b'\n' && r#type != Type::BlockQuote {
                if r#type != Type::Spoiler
                    || !(nested_entities.last().unwrap().entity_byte_offset == i - 2
                        || (nested_entities.last().unwrap().entity_byte_offset == i - 3
                            && !result.is_empty()
                            && result[result.len() - 1] == b'\r'))
                {
                    let back = nested_entities.last().unwrap();
                    return Err(Status::error(
                        400,
                        &format!(
                            "Can't find end of {} entity at byte offset {}",
                            back.r#type, back.entity_byte_offset
                        ),
                    ));
                }
                nested_entities.pop();
                assert!(
                    !nested_entities.is_empty(),
                    "the C++ CHECK(!nested_entities.empty())"
                );
                r#type = nested_entities.last().unwrap().r#type;
                if r#type != Type::BlockQuote {
                    assert_ne!(
                        r#type,
                        Type::Spoiler,
                        "the C++ CHECK(type != Type::Spoiler)"
                    );
                    let back = nested_entities.last().unwrap();
                    return Err(Status::error(
                        400,
                        &format!(
                            "Can't find end of {} entity at byte offset {}",
                            back.r#type, back.entity_byte_offset
                        ),
                    ));
                }
                r#type = Type::ExpandableBlockQuote;
            }
            let mut argument = std::mem::take(&mut nested_entities.last_mut().unwrap().argument);
            let mut user_id = UserId::default();
            let mut custom_emoji_id = CustomEmojiId::default();
            let mut date = FormattedDate::default();
            let mut skip_entity = utf16_offset == nested_entities.last().unwrap().entity_offset;
            match r#type {
                Type::Bold | Type::Italic | Type::Code | Type::Strikethrough => {}
                Type::Underline | Type::Spoiler => {
                    i += 1;
                }
                Type::Pre | Type::PreCode => {
                    i += 2;
                }
                Type::TextUrl => {
                    let url = if at(i + 1) != b'(' {
                        // use text as a URL — the compacted output so far
                        let begin = nested_entities.last().unwrap().entity_begin_pos;
                        String::from_utf8(result[begin..].to_vec())
                            .expect("the compacted output is valid UTF-8")
                    } else {
                        i += 2;
                        let url_begin_pos = i;
                        let mut url_bytes = Vec::new();
                        while i < text.len() && at(i) != b')' {
                            if at(i) == b'\\' && at(i + 1) > 0 && at(i + 1) <= 126 {
                                url_bytes.push(at(i + 1));
                                i += 2;
                                continue;
                            }
                            url_bytes.push(at(i));
                            i += 1;
                        }
                        if at(i) != b')' {
                            return Err(Status::error(
                                400,
                                &format!(
                                    "Can't find end of a URL at byte offset {}",
                                    url_begin_pos
                                ),
                            ));
                        }
                        String::from_utf8(url_bytes).expect("a URL is built from whole characters")
                    };
                    user_id = crate::link_manager::get_link_user_id(&url);
                    if !user_id.is_valid() {
                        let checked = crate::link_manager::get_checked_link(&url);
                        if checked.is_empty() {
                            skip_entity = true;
                        } else {
                            argument = checked;
                        }
                    }
                }
                Type::CustomEmoji => {
                    if at(i + 1) != b'(' {
                        return Err(Status::error(
                            400,
                            "The entity must contain a tg://emoji or tg://time URL",
                        ));
                    }
                    i += 2;
                    let mut url_bytes = Vec::new();
                    let url_begin_pos = i;
                    while i < text.len() && at(i) != b')' {
                        if at(i) == b'\\' && at(i + 1) > 0 && at(i + 1) <= 126 {
                            url_bytes.push(at(i + 1));
                            i += 2;
                            continue;
                        }
                        url_bytes.push(at(i));
                        i += 1;
                    }
                    if at(i) != b')' {
                        return Err(Status::error(
                            400,
                            &format!("Can't find end of a URL at byte offset {}", url_begin_pos),
                        ));
                    }
                    let url =
                        String::from_utf8(url_bytes).expect("a URL is built from whole characters");
                    match crate::link_manager::get_link_custom_emoji_id(&url) {
                        Ok(found_custom_emoji_id) => custom_emoji_id = found_custom_emoji_id,
                        Err(_) => match crate::link_manager::get_link_formatted_date(&url) {
                            Ok(found_date) => {
                                date = found_date;
                                r#type = Type::FormattedDate;
                            }
                            Err(_) => {
                                return Err(Status::error(
                                    400,
                                    "Invalid tg://emoji or tg://time URL specified",
                                ));
                            }
                        },
                    }
                }
                Type::BlockQuote | Type::ExpandableBlockQuote => {
                    assert!(have_blockquote, "the C++ CHECK(have_blockquote)");
                    have_blockquote = false;
                    result.push(at(i));
                    can_start_blockquote = true;
                    utf16_offset += 1;
                    skip_entity = false;
                }
                _ => unreachable!("the C++ UNREACHABLE()"),
            }

            if !skip_entity {
                let back = nested_entities.last().unwrap();
                let entity_offset = back.entity_offset;
                let entity_length = utf16_offset - entity_offset;
                if user_id.is_valid() {
                    entities.push(MessageEntity::mention_name(
                        entity_offset,
                        entity_length,
                        user_id,
                    ));
                } else if custom_emoji_id.is_valid() {
                    entities.push(MessageEntity::custom_emoji(
                        entity_offset,
                        entity_length,
                        custom_emoji_id,
                    ));
                } else if date.is_valid() {
                    entities.push(MessageEntity::formatted_date(
                        entity_offset,
                        entity_length,
                        date,
                    ));
                } else {
                    entities.push(MessageEntity::with_argument(
                        r#type,
                        entity_offset,
                        entity_length,
                        argument,
                    ));
                }
            }
            nested_entities.pop();
        }
        i += 1;
    }
    if have_blockquote {
        assert!(
            !nested_entities.is_empty(),
            "the C++ CHECK(!nested_entities.empty())"
        );
        let mut r#type = Type::BlockQuote;
        if nested_entities.last().unwrap().r#type == Type::Spoiler
            && nested_entities.last().unwrap().entity_byte_offset == text.len() - 2
        {
            nested_entities.pop();
            assert!(
                !nested_entities.is_empty(),
                "the C++ CHECK(!nested_entities.empty())"
            );
            r#type = Type::ExpandableBlockQuote;
        }
        if nested_entities.last().unwrap().r#type == Type::BlockQuote {
            have_blockquote = false;
            let back = nested_entities.last().unwrap();
            let entity_offset = back.entity_offset;
            let entity_length = utf16_offset - entity_offset;
            if entity_length != 0 {
                entities.push(MessageEntity::new(r#type, entity_offset, entity_length));
            }
            nested_entities.pop();
        }
    }
    if let Some(back) = nested_entities.last() {
        return Err(Status::error(
            400,
            &format!(
                "Can't find end of {} entity at byte offset {}",
                back.r#type, back.entity_byte_offset
            ),
        ));
    }

    sort_entities(&mut entities);

    *text = String::from_utf8(result).expect("parse_markdown_v2 preserves UTF-8");
    Ok(entities)
}

// ===== M.8.8 — Markdown v3 (MessageEntity.cpp:2401-3238).
// The v3 dialect is present in the pinned upstream tree (d1085f9ce) that
// this port tracks; see the recorded version-drift decision. =====

/// `find_text_url_entities_v3` (MessageEntity.cpp:2401-2443): the
/// alternating `[text`, `(url)` slice pairs whose URL passes
/// `get_checked_link`.
fn find_text_url_entities_v3(text: &str) -> Vec<(&str, &str)> {
    let mut result = Vec::new();
    let bytes = text.as_bytes();
    let size = bytes.len();
    let mut i = 0usize;
    while i < size {
        if bytes[i] != b'[' {
            i += 1;
            continue;
        }

        let text_begin = i;
        let mut text_end = text_begin + 1;
        while text_end < size && bytes[text_end] != b']' {
            text_end += 1;
        }

        i = text_end; // prevent quadratic asymptotic

        if text_end == size || text_end == text_begin + 1 {
            continue;
        }

        let url_begin = text_end + 1;
        if url_begin == size || bytes[url_begin] != b'(' {
            continue;
        }

        let mut url_end = url_begin + 1;
        while url_end < size && bytes[url_end] != b')' {
            url_end += 1;
        }

        i = url_end; // prevent quadratic asymptotic, disallows [a](b[c](t.me)

        if url_end < size {
            let url = &text[url_begin + 1..url_end];
            if !crate::link_manager::get_checked_link(url).is_empty() {
                result.push((
                    &text[text_begin..text_end + 1],
                    &text[url_begin..url_end + 1],
                ));
            }
        }
    }
    result
}

/// `parse_text_url_entities_v3` (MessageEntity.cpp:2445-2622): `entities`
/// must be valid for the text.
fn parse_text_url_entities_v3(text: &str, entities: Vec<MessageEntity>) -> FormattedText {
    // continuous entities can't intersect TextUrl entities,
    // so try to find new TextUrl entities only between the predetermined continuous entities

    let mut left_text = text;
    let mut result = FormattedText::default();
    let mut result_text_utf16_length = 0i32;
    let mut part_entities: Vec<MessageEntity> = Vec::new();
    let mut part_splittable_entities: [Vec<MessageEntity>; SPLITTABLE_ENTITY_TYPE_COUNT] =
        [Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new()];
    let mut part_begin = 0i32;
    let mut max_end = 0i32;
    let mut skipped_length = 0i32;

    #[allow(clippy::too_many_arguments)] // mirrors the C++ add_part lambda
    fn add_part(
        left_text: &mut &str,
        result: &mut FormattedText,
        result_text_utf16_length: &mut i32,
        part_entities: &mut Vec<MessageEntity>,
        part_splittable_entities: &mut [Vec<MessageEntity>; SPLITTABLE_ENTITY_TYPE_COUNT],
        part_begin: &mut i32,
        max_end: &mut i32,
        skipped_length: &mut i32,
        part_end: i32,
    ) {
        // we have [part_begin, max_end) kept part and [max_end, part_end) part to parse text_url entities

        if *max_end != *part_begin {
            // add all entities from the kept part
            let kept_part_text = tdutils::utf8::utf8_utf16_substr_range(
                left_text,
                0,
                (*max_end - *part_begin) as usize,
            );
            *left_text = &(*left_text)[kept_part_text.len()..];

            result.text.push_str(kept_part_text);
            result.entities.append(part_entities);
            *result_text_utf16_length += *max_end - *part_begin;
        }

        let mut splittable_entity_pos = [0usize; SPLITTABLE_ENTITY_TYPE_COUNT];
        for splittable_entities in part_splittable_entities.iter() {
            check_non_intersecting(splittable_entities);
        }
        if part_end != *max_end {
            // try to find text_url entities in the left part
            let parsed_part_text = tdutils::utf8::utf8_utf16_substr_range(
                left_text,
                0,
                (part_end - *max_end) as usize,
            );
            *left_text = &(*left_text)[parsed_part_text.len()..];

            let text_urls = find_text_url_entities_v3(parsed_part_text);

            let mut text_utf16_offset = *max_end;
            let mut prev_pos = 0usize;
            // The C++ iterates a flat vector in steps of 2; the port packs
            // each (text, url) find into one pair, so step by 1.
            let mut pair = 0usize;
            while pair < text_urls.len() {
                let text_slice = text_urls[pair].0;
                let url_slice = text_urls[pair].1;
                let text_begin_pos = slice_offset(parsed_part_text, text_slice) as usize;
                let text_end_pos = text_begin_pos + text_slice.len() - 1;
                let url_begin_pos = slice_offset(parsed_part_text, url_slice) as usize;
                let url_end_pos = url_begin_pos + url_slice.len() - 1;
                assert_eq!(
                    parsed_part_text.as_bytes()[text_begin_pos],
                    b'[',
                    "the C++ CHECK"
                );
                assert_eq!(
                    parsed_part_text.as_bytes()[text_end_pos],
                    b']',
                    "the C++ CHECK"
                );
                assert_eq!(url_begin_pos, text_end_pos + 1, "the C++ CHECK");
                assert_eq!(
                    parsed_part_text.as_bytes()[url_begin_pos],
                    b'(',
                    "the C++ CHECK"
                );
                assert_eq!(
                    parsed_part_text.as_bytes()[url_end_pos],
                    b')',
                    "the C++ CHECK"
                );

                let before_text_url = &parsed_part_text[prev_pos..text_begin_pos];
                let before_text_url_utf16_length = text_length(before_text_url);
                *result_text_utf16_length += before_text_url_utf16_length;
                result.text.push_str(before_text_url);
                text_utf16_offset += before_text_url_utf16_length;

                let text_url = &parsed_part_text[text_begin_pos + 1..text_end_pos];
                let text_url_utf16_length = text_length(text_url);
                let url = &parsed_part_text[url_begin_pos + 1..url_end_pos];
                let url_utf16_length = text_length(url);
                result.entities.push(MessageEntity::with_argument(
                    Type::TextUrl,
                    *result_text_utf16_length,
                    text_url_utf16_length,
                    crate::link_manager::get_checked_link(url),
                ));
                result.text.push_str(text_url);
                *result_text_utf16_length += text_url_utf16_length;

                let initial_utf16_length = 1 + text_url_utf16_length + 1 + 1 + url_utf16_length + 1;

                // adjust splittable entities, removing deleted parts from them
                // in the segment [text_utf16_offset, text_utf16_offset + initial_utf16_length)
                // the first character and the last (url_utf16_length + 3) characters are deleted
                for index in 0..SPLITTABLE_ENTITY_TYPE_COUNT {
                    let mut pos = splittable_entity_pos[index];
                    let splittable_entities = &mut part_splittable_entities[index];
                    while pos < splittable_entities.len()
                        && splittable_entities[pos].offset
                            < text_utf16_offset + initial_utf16_length
                    {
                        let mut offset = splittable_entities[pos].offset;
                        let mut length = splittable_entities[pos].length;
                        if offset + length > text_utf16_offset + 1 + text_url_utf16_length {
                            // ends after last removed part; truncate length
                            length = text_utf16_offset + 1 + text_url_utf16_length - offset;
                        }
                        if offset > text_utf16_offset {
                            offset -= 1;
                        } else if offset + length > text_utf16_offset {
                            length -= 1;
                        }
                        if length > 0 {
                            assert!(
                                offset >= *skipped_length,
                                "the C++ CHECK(offset >= skipped_length)"
                            );
                            assert!(
                                offset - *skipped_length + length <= *result_text_utf16_length,
                                "the C++ CHECK(offset - skipped_length + length <= result_text_utf16_length)"
                            );
                            if offset < text_utf16_offset && offset + length > text_utf16_offset {
                                // entity intersects start on the new text_url entity; split it
                                result.entities.push(MessageEntity::new(
                                    entity_type(&splittable_entities[pos]),
                                    offset - *skipped_length,
                                    text_utf16_offset - offset,
                                ));
                                length -= text_utf16_offset - offset;
                                offset = text_utf16_offset;
                            }
                            result.entities.push(MessageEntity::new(
                                entity_type(&splittable_entities[pos]),
                                offset - *skipped_length,
                                length,
                            ));
                        }
                        if splittable_entities[pos].offset + splittable_entities[pos].length
                            > text_utf16_offset + initial_utf16_length
                        {
                            // begins before end of the segment, but ends after it
                            // need to keep the entity for future segments, so split the entity
                            splittable_entities[pos].length = splittable_entities[pos].offset
                                + splittable_entities[pos].length
                                - (text_utf16_offset + initial_utf16_length);
                            splittable_entities[pos].offset =
                                text_utf16_offset + initial_utf16_length;
                        } else {
                            pos += 1;
                        }
                    }
                    splittable_entity_pos[index] = pos;
                }
                text_utf16_offset += initial_utf16_length;

                *skipped_length += 2 + 2 + url_utf16_length;
                prev_pos = url_end_pos + 1;

                pair += 1;
            }

            result.text.push_str(&parsed_part_text[prev_pos..]);
            *result_text_utf16_length += part_end - text_utf16_offset;
        }

        // now add all left splittable entities from [part_begin, part_end)
        for index in 0..SPLITTABLE_ENTITY_TYPE_COUNT {
            let mut pos = splittable_entity_pos[index];
            let splittable_entities = &mut part_splittable_entities[index];
            while pos < splittable_entities.len() && splittable_entities[pos].offset < part_end {
                if splittable_entities[pos].offset + splittable_entities[pos].length > part_end {
                    // begins before end of the segment, but ends after it
                    // need to keep the entity for future segments, so split the entity
                    // entities don't intersect each other, so there can be at most one such entity
                    result.entities.push(MessageEntity::new(
                        entity_type(&splittable_entities[pos]),
                        splittable_entities[pos].offset - *skipped_length,
                        part_end - splittable_entities[pos].offset,
                    ));

                    splittable_entities[pos].length = splittable_entities[pos].offset
                        + splittable_entities[pos].length
                        - part_end;
                    splittable_entities[pos].offset = part_end;
                } else {
                    result.entities.push(MessageEntity::new(
                        entity_type(&splittable_entities[pos]),
                        splittable_entities[pos].offset - *skipped_length,
                        splittable_entities[pos].length,
                    ));
                    pos += 1;
                }
            }
            if pos == splittable_entities.len() {
                splittable_entities.clear();
            } else {
                assert_eq!(pos, splittable_entities.len() - 1, "the C++ CHECK");
                debug_assert!(!left_text.is_empty(), "the C++ LOG_CHECK(!text.empty())");
                splittable_entities[0] = splittable_entities.pop().unwrap();
            }
        }

        *part_begin = part_end;
    }

    for entity in entities {
        if is_splittable_entity(entity_type(&entity)) {
            let index = splittable_entity_type_index(entity_type(&entity));
            part_splittable_entities[index].push(entity);
            continue;
        }
        assert!(
            is_continuous_entity(entity_type(&entity)),
            "the C++ CHECK(is_continuous_entity)"
        );

        if entity.offset > max_end {
            // found a gap from max_end to entity.offset between predetermined entities
            add_part(
                &mut left_text,
                &mut result,
                &mut result_text_utf16_length,
                &mut part_entities,
                &mut part_splittable_entities,
                &mut part_begin,
                &mut max_end,
                &mut skipped_length,
                entity.offset,
            );
        } else {
            assert_eq!(
                entity.offset, max_end,
                "the C++ CHECK(entity.offset == max_end)"
            );
        }

        max_end = entity.offset + entity.length;
        let mut entity = entity;
        entity.offset -= skipped_length;
        part_entities.push(entity);
    }
    let final_part_end = part_begin + text_length(text);
    add_part(
        &mut left_text,
        &mut result,
        &mut result_text_utf16_length,
        &mut part_entities,
        &mut part_splittable_entities,
        &mut part_begin,
        &mut max_end,
        &mut skipped_length,
        final_part_end,
    );

    result
}

/// `find_splittable_entities_v3` (MessageEntity.cpp:2624-2697).
fn find_splittable_entities_v3(text: &str, entities: &[MessageEntity]) -> Vec<MessageEntity> {
    let mut unallowed_boundaries: std::collections::HashSet<i32> = std::collections::HashSet::new();
    for entity in entities {
        unallowed_boundaries.insert(entity.offset + 1);
        unallowed_boundaries.insert(entity.offset + entity.length + 1);
        let r#type = entity_type(entity);
        if matches!(
            r#type,
            Type::Mention
                | Type::Hashtag
                | Type::BotCommand
                | Type::Cashtag
                | Type::PhoneNumber
                | Type::BankCardNumber
        ) {
            for i in 1..entity.length {
                unallowed_boundaries.insert(entity.offset + i + 1);
            }
        }
    }

    let mut found_entities = find_entities(text, false, true);
    found_entities.retain(|entity| !matches!(entity_type(entity), Type::EmailAddress | Type::Url));
    for entity in &found_entities {
        for i in 0..=entity.length {
            unallowed_boundaries.insert(entity.offset + i + 1);
        }
    }

    let bytes = text.as_bytes();
    let mut result: Vec<MessageEntity> = Vec::new();
    let mut splittable_entity_offset = [0i32; SPLITTABLE_ENTITY_TYPE_COUNT];
    let mut utf16_offset = 0i32;
    let mut i = 0usize;
    while i + 1 < bytes.len() {
        let c = bytes[i];
        if tdutils::utf8::is_utf8_character_first_code_unit(c) {
            utf16_offset += 1 + i32::from(c >= 0xf0); // >= 4 bytes in symbol => surrogate pair
        }
        if (c == b'_' || c == b'*' || c == b'~' || c == b'|')
            && bytes[i] == bytes[i + 1]
            && !unallowed_boundaries.contains(&(utf16_offset + 1))
        {
            let mut j = i + 2;
            while j != bytes.len()
                && bytes[j] == bytes[i]
                && !unallowed_boundaries.contains(&(utf16_offset + (j - i) as i32))
            {
                j += 1;
            }
            if j == i + 2 {
                let r#type = match c {
                    b'_' => Type::Italic,
                    b'*' => Type::Bold,
                    b'~' => Type::Strikethrough,
                    b'|' => Type::Spoiler,
                    _ => unreachable!("the C++ UNREACHABLE()"),
                };
                let index = splittable_entity_type_index(r#type);
                if splittable_entity_offset[index] != 0 {
                    let length = utf16_offset - splittable_entity_offset[index] - 1;
                    if length > 0 {
                        result.push(MessageEntity::new(
                            r#type,
                            splittable_entity_offset[index],
                            length,
                        ));
                    }
                    splittable_entity_offset[index] = 0;
                } else {
                    splittable_entity_offset[index] = utf16_offset + 1;
                }
            }
            utf16_offset += (j - i - 1) as i32;
            i = j - 1;
        }
        i += 1;
    }
    result
}

/// `parse_markdown_v3_without_pre` (MessageEntity.cpp:2699-2768):
/// entities must be valid and can contain only splittable and continuous
/// entities; `__italic__ ~~strikethrough~~ **bold** ||spoiler||` and
/// `[text_url](telegram.org)` entities are left to be parsed.
fn parse_markdown_v3_without_pre(text: &str, mut entities: Vec<MessageEntity>) -> FormattedText {
    // `text_owner` is always overwritten before it is read on the branch
    // that reads it.
    #[allow(unused_assignments)]
    let mut text_owner: Option<String> = None;
    let mut text = text;
    check_is_sorted(&entities);

    if text.contains('[') {
        let parsed = parse_text_url_entities_v3(text, entities);
        let FormattedText {
            text: parsed_text,
            entities: parsed_entities,
        } = parsed;
        text_owner = Some(parsed_text);
        text = text_owner.as_deref().expect("just set");
        entities = parsed_entities;
    }
    // splittable entities are sorted only within a fixed type now

    let have_splittable_entities = {
        let bytes = text.as_bytes();
        let mut found = false;
        for i in 0..bytes.len().saturating_sub(1) {
            if (bytes[i] == b'_' || bytes[i] == b'*' || bytes[i] == b'~' || bytes[i] == b'|')
                && bytes[i] == bytes[i + 1]
            {
                found = true;
                break;
            }
        }
        found
    };
    if !have_splittable_entities {
        // fast path
        sort_entities(&mut entities);
        return FormattedText {
            text: text.to_owned(),
            entities,
        };
    }

    let found_splittable_entities = find_splittable_entities_v3(text, &entities);
    let mut removed_pos: Vec<i32> = Vec::new();
    for entity in &found_splittable_entities {
        removed_pos.push(entity.offset - 1);
        removed_pos.push(entity.offset + entity.length + 1);
    }
    removed_pos.sort_unstable();

    let bytes = text.as_bytes();
    let mut new_text: Vec<u8> = Vec::new();
    assert!(
        bytes.len() >= 2 * removed_pos.len(),
        "the C++ CHECK(text.size() >= 2 * removed_pos.size())"
    );
    new_text.reserve(bytes.len() - 2 * removed_pos.len());
    let mut j = 0usize;
    let mut utf16_offset = 0i32;
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i];
        if tdutils::utf8::is_utf8_character_first_code_unit(c) {
            utf16_offset += 1 + i32::from(c >= 0xf0); // >= 4 bytes in symbol => surrogate pair
        }
        if j < removed_pos.len() && utf16_offset == removed_pos[j] {
            i += 1;
            utf16_offset += 1;
            assert!(
                j + 1 == removed_pos.len() || removed_pos[j + 1] >= removed_pos[j] + 2,
                "the C++ CHECK"
            );
            j += 1;
        } else {
            new_text.push(c);
        }
        i += 1;
    }
    assert_eq!(
        j,
        removed_pos.len(),
        "the C++ CHECK(j == removed_pos.size())"
    );
    entities.extend(found_splittable_entities);
    for entity in entities.iter_mut() {
        let removed_before_begin = removed_pos.partition_point(|&pos| pos <= entity.offset) as i32;
        let removed_before_end =
            removed_pos.partition_point(|&pos| pos <= entity.offset + entity.length) as i32;
        entity.length -= 2 * (removed_before_end - removed_before_begin);
        entity.offset -= 2 * removed_before_begin;
        assert!(entity.offset >= 0, "the C++ CHECK(entity.offset >= 0)");
        assert!(entity.length >= 0, "the C++ CHECK(entity.length >= 0)");
        assert!(
            entity.offset + entity.length <= utf16_offset,
            "the C++ CHECK(entity.offset + entity.length <= utf16_offset)"
        );
    }

    remove_empty_entities(&mut entities);

    sort_entities(&mut entities);
    FormattedText {
        text: String::from_utf8(new_text).expect("the text is a copy of valid UTF-8 input"),
        entities,
    }
}

/// `parse_pre_entities_v3(Slice)` (MessageEntity.cpp:2770-2852).
fn parse_pre_entities_v3(text: &str) -> FormattedText {
    let mut result: Vec<u8> = Vec::new();
    let mut entities: Vec<MessageEntity> = Vec::new();
    let bytes = text.as_bytes();
    let size = bytes.len();
    let mut utf16_offset = 0i32;
    let mut i = 0usize;
    while i < size {
        let c = bytes[i];
        if c != b'`' {
            if tdutils::utf8::is_utf8_character_first_code_unit(c) {
                utf16_offset += 1 + i32::from(c >= 0xf0); // >= 4 bytes in symbol => surrogate pair
            }
            result.push(bytes[i]);
            i += 1;
            continue;
        }

        let mut j = i + 1;
        while j < size && bytes[j] == b'`' {
            j += 1;
        }

        if j - i == 1 || j - i == 3 {
            // trying to find end of the entity
            let mut entity_length = 0i32;
            let mut is_found = false;
            let mut end_tag_begin = j;
            while end_tag_begin < size {
                let cur_c = bytes[end_tag_begin];
                if cur_c == b'`' {
                    // possible end tag
                    let mut end_tag_end = end_tag_begin + 1;
                    while end_tag_end < size && bytes[end_tag_end] == b'`' {
                        end_tag_end += 1;
                    }
                    if end_tag_end - end_tag_begin == j - i {
                        // end tag found
                        assert!(entity_length > 0, "the C++ CHECK(entity_length > 0)");
                        let mut entity_begin = j;
                        let mut language_code = String::new();
                        if j - i == 3 {
                            let mut language_code_end = j;
                            while language_code_end < end_tag_begin - 1
                                && (33..=126).contains(&bytes[language_code_end])
                            {
                                language_code_end += 1;
                            }
                            if language_code_end < end_tag_begin - 1
                                && bytes[language_code_end] == b'\n'
                            {
                                language_code = text[entity_begin..language_code_end].to_owned();
                                entity_begin = language_code_end + 1;
                                entity_length -= (entity_begin - j) as i32;
                                assert!(entity_length > 0, "the C++ CHECK(entity_length > 0)");
                            }
                        }
                        if !language_code.is_empty() {
                            entities.push(MessageEntity::with_argument(
                                Type::PreCode,
                                utf16_offset,
                                entity_length,
                                language_code,
                            ));
                        } else {
                            entities.push(MessageEntity::new(
                                if j - i == 3 { Type::Pre } else { Type::Code },
                                utf16_offset,
                                entity_length,
                            ));
                        }
                        result.extend_from_slice(&text.as_bytes()[entity_begin..end_tag_begin]);
                        utf16_offset += entity_length;
                        i = end_tag_end - 1;
                        is_found = true;
                        break;
                    } else {
                        // not an end tag, skip
                        entity_length += (end_tag_end - end_tag_begin) as i32;
                        end_tag_begin = end_tag_end - 1;
                    }
                } else if tdutils::utf8::is_utf8_character_first_code_unit(cur_c) {
                    entity_length += 1 + i32::from(cur_c >= 0xf0); // >= 4 bytes in symbol => surrogate pair
                }
                end_tag_begin += 1;
            }
            if is_found {
                i += 1;
                continue;
            }
        }

        result.extend_from_slice(&text.as_bytes()[i..j]);
        utf16_offset += (j - i) as i32;
        i = j;
    }
    FormattedText {
        text: String::from_utf8(result).expect("the text is a copy of valid UTF-8 input"),
        entities,
    }
}

/// `parse_pre_entities_v3(Slice, vector<MessageEntity>)`
/// (MessageEntity.cpp:2854-2917): entities must be valid for the text.
fn parse_pre_entities_v3_with_entities(text: &str, entities: Vec<MessageEntity>) -> FormattedText {
    // nothing can intersect pre entities, so ignore all '`' inside the predetermined entities
    // and try to find new pre entities only between the predetermined entities

    let mut left_text = text;
    let mut result = FormattedText::default();
    let mut result_text_utf16_length = 0i32;
    let mut part_begin = 0i32;
    let mut max_end = 0i32;
    let mut skipped_length = 0i32;

    fn add_part(
        left_text: &mut &str,
        result: &mut FormattedText,
        result_text_utf16_length: &mut i32,
        part_begin: &mut i32,
        max_end: &mut i32,
        skipped_length: &mut i32,
        part_end: i32,
    ) {
        // we have [part_begin, max_end) kept part and [max_end, part_end) part to parse pre entities
        assert_eq!(
            *part_begin,
            *result_text_utf16_length + *skipped_length,
            "the C++ CHECK"
        );

        if *max_end != *part_begin {
            // add the kept part
            let kept_part_text = tdutils::utf8::utf8_utf16_substr_range(
                left_text,
                0,
                (*max_end - *part_begin) as usize,
            );
            *left_text = &(*left_text)[kept_part_text.len()..];

            result.text.push_str(kept_part_text);
            *result_text_utf16_length += *max_end - *part_begin;
        }

        if part_end != *max_end {
            // try to find pre entities in the left part
            let parsed_part_text = tdutils::utf8::utf8_utf16_substr_range(
                left_text,
                0,
                (part_end - *max_end) as usize,
            );
            *left_text = &(*left_text)[parsed_part_text.len()..];

            if !parsed_part_text.contains('`') {
                // fast path, no pre entities; just append the text
                result.text.push_str(parsed_part_text);
                *result_text_utf16_length += part_end - *max_end;
            } else {
                let mut parsed_text = parse_pre_entities_v3(parsed_part_text);
                let new_skipped_length = (parsed_part_text.len() - parsed_text.text.len()) as i32;
                assert!(
                    new_skipped_length < part_end - *max_end,
                    "the C++ CHECK(new_skipped_length < part_end - max_end)"
                );
                result.text.push_str(&parsed_text.text);
                for entity in parsed_text.entities.iter_mut() {
                    entity.offset += *result_text_utf16_length;
                }
                result.entities.append(&mut parsed_text.entities);
                *result_text_utf16_length += part_end - *max_end - new_skipped_length;
                *skipped_length += new_skipped_length;
            }
        }

        *part_begin = part_end;
    }

    for mut entity in entities {
        if entity.offset > max_end {
            // found a gap from max_end to entity.offset between predetermined entities
            add_part(
                &mut left_text,
                &mut result,
                &mut result_text_utf16_length,
                &mut part_begin,
                &mut max_end,
                &mut skipped_length,
                entity.offset,
            );
        }

        max_end = max_end.max(entity.offset + entity.length);
        entity.offset -= skipped_length;
        result.entities.push(entity);
    }
    let final_part_end = part_begin + text_length(text);
    add_part(
        &mut left_text,
        &mut result,
        &mut result_text_utf16_length,
        &mut part_begin,
        &mut max_end,
        &mut skipped_length,
        final_part_end,
    );

    result
}

/// `parse_markdown_v3_without_blockquote` (MessageEntity.cpp:2919-2990):
/// entities must be valid and can contain only splittable, continuous and
/// pre entities.
fn parse_markdown_v3_without_blockquote(text: FormattedText) -> FormattedText {
    let mut text = text;
    if text.text.contains('`') {
        text = parse_pre_entities_v3_with_entities(&text.text, text.entities);
        check_is_sorted(&text.entities);
    }

    let have_pre = text
        .entities
        .iter()
        .any(|entity| is_pre_entity(entity_type(entity)));
    if !have_pre {
        // fast path
        return parse_markdown_v3_without_pre(&text.text, text.entities);
    }

    let mut result = FormattedText::default();
    let mut result_text_utf16_length = 0i32;
    let mut part_entities: Vec<MessageEntity> = Vec::new();
    let mut part_begin = 0i32;
    let mut max_end = 0i32;
    let mut left_text: &str = &text.text;

    fn add_part(
        left_text: &mut &str,
        result: &mut FormattedText,
        result_text_utf16_length: &mut i32,
        part_entities: &mut Vec<MessageEntity>,
        part_begin: &mut i32,
        part_end: i32,
    ) {
        let part_text =
            tdutils::utf8::utf8_utf16_substr_range(left_text, 0, (part_end - *part_begin) as usize);
        *left_text = &(*left_text)[part_text.len()..];

        let mut part = parse_markdown_v3_without_pre(part_text, std::mem::take(part_entities));

        result.text.push_str(&part.text);
        for entity in part.entities.iter_mut() {
            entity.offset += *result_text_utf16_length;
        }
        result.entities.append(&mut part.entities);
        *result_text_utf16_length += text_length(&part.text);
        *part_begin = part_end;
    }

    for i in 0..text.entities.len() {
        let entity = text.entities[i].clone();
        assert!(
            is_splittable_entity(entity_type(&entity))
                || is_pre_entity(entity_type(&entity))
                || is_continuous_entity(entity_type(&entity)),
            "the C++ CHECK"
        );
        if is_pre_entity(entity_type(&entity)) {
            assert!(
                entity.offset >= max_end,
                "the C++ CHECK(entity.offset >= max_end)"
            );
            assert!(
                i + 1 == text.entities.len()
                    || text.entities[i + 1].offset >= entity.offset + entity.length,
                "the C++ CHECK"
            );

            add_part(
                &mut left_text,
                &mut result,
                &mut result_text_utf16_length,
                &mut part_entities,
                &mut part_begin,
                entity.offset,
            );

            let part_text =
                tdutils::utf8::utf8_utf16_substr_range(left_text, 0, entity.length as usize);
            left_text = &left_text[part_text.len()..];

            result.text.push_str(part_text);
            let mut entity = entity;
            entity.offset = result_text_utf16_length;
            result.entities.push(entity);
            result_text_utf16_length += text.entities[i].length;
            part_begin = text.entities[i].offset + text.entities[i].length;
        } else {
            assert!(
                entity.offset >= part_begin,
                "the C++ CHECK(entity.offset >= part_begin)"
            );
            let mut entity = entity;
            entity.offset -= part_begin;
            part_entities.push(entity);
        }

        max_end = max_end.max(text.entities[i].offset + text.entities[i].length);
    }
    let final_part_end = part_begin + text_length(left_text);
    add_part(
        &mut left_text,
        &mut result,
        &mut result_text_utf16_length,
        &mut part_entities,
        &mut part_begin,
        final_part_end,
    );

    result
}

/// `parse_markdown_v3` (MessageEntity.cpp:2992-3068): text entities must be
/// valid; returned entities must be resplit and fixed.
pub fn parse_markdown_v3(text: FormattedText) -> FormattedText {
    let have_blockquote = text
        .entities
        .iter()
        .any(|entity| is_blockquote_entity(entity_type(entity)));
    if !have_blockquote {
        // fast path
        return parse_markdown_v3_without_blockquote(text);
    }

    let mut result = FormattedText::default();
    let mut result_text_utf16_length = 0i32;
    let mut part_entities: Vec<MessageEntity> = Vec::new();
    let mut part_begin = 0i32;
    let mut max_end = 0i32;
    let mut left_text: &str = &text.text;

    fn add_part(
        left_text: &mut &str,
        result: &mut FormattedText,
        result_text_utf16_length: &mut i32,
        part_entities: &mut Vec<MessageEntity>,
        part_begin: &mut i32,
        part_end: i32,
    ) {
        let part_text =
            tdutils::utf8::utf8_utf16_substr_range(left_text, 0, (part_end - *part_begin) as usize);
        *left_text = &(*left_text)[part_text.len()..];

        let mut part = parse_markdown_v3_without_blockquote(FormattedText {
            text: part_text.to_owned(),
            entities: std::mem::take(part_entities),
        });

        result.text.push_str(&part.text);
        for entity in part.entities.iter_mut() {
            entity.offset += *result_text_utf16_length;
        }
        result.entities.append(&mut part.entities);
        *result_text_utf16_length += text_length(&part.text);
        *part_begin = part_end;
    }

    let mut i = 0usize;
    while i < text.entities.len() {
        let entity = text.entities[i].clone();
        let entity_offset = entity.offset;
        let entity_length = entity.length;
        assert!(
            is_splittable_entity(entity_type(&entity))
                || is_pre_entity(entity_type(&entity))
                || is_continuous_entity(entity_type(&entity))
                || is_blockquote_entity(entity_type(&entity)),
            "the C++ CHECK"
        );
        if is_blockquote_entity(entity_type(&entity)) {
            assert!(
                entity_offset >= max_end,
                "the C++ CHECK(entity.offset >= max_end)"
            );

            add_part(
                &mut left_text,
                &mut result,
                &mut result_text_utf16_length,
                &mut part_entities,
                &mut part_begin,
                entity_offset,
            );

            let offset = result_text_utf16_length;
            let mut blockquote_entity = entity;
            blockquote_entity.offset = offset;
            result.entities.push(blockquote_entity);
            let index = result.entities.len() - 1;

            while i + 1 < text.entities.len()
                && text.entities[i + 1].offset + text.entities[i + 1].length
                    <= entity_offset + entity_length
            {
                i += 1;
                let mut next_entity = text.entities[i].clone();
                assert!(
                    is_splittable_entity(entity_type(&next_entity))
                        || is_pre_entity(entity_type(&next_entity))
                        || is_continuous_entity(entity_type(&next_entity)),
                    "the C++ CHECK"
                );
                assert!(
                    next_entity.offset >= part_begin,
                    "the C++ CHECK(next_entity.offset >= part_begin)"
                );
                next_entity.offset -= part_begin;
                part_entities.push(next_entity);
            }
            assert!(
                i + 1 == text.entities.len()
                    || text.entities[i + 1].offset >= entity_offset + entity_length,
                "the C++ CHECK"
            );
            add_part(
                &mut left_text,
                &mut result,
                &mut result_text_utf16_length,
                &mut part_entities,
                &mut part_begin,
                entity_offset + entity_length,
            );

            result.entities[index].length = result_text_utf16_length - offset;
        } else {
            assert!(
                entity_offset >= part_begin,
                "the C++ CHECK(entity.offset >= part_begin)"
            );
            let mut entity = entity;
            entity.offset -= part_begin;
            part_entities.push(entity);
        }

        max_end = max_end.max(entity_offset + entity_length);
        i += 1;
    }
    let final_part_end = part_begin + text_length(left_text);
    add_part(
        &mut left_text,
        &mut result,
        &mut result_text_utf16_length,
        &mut part_entities,
        &mut part_begin,
        final_part_end,
    );

    result
}

/// `get_markdown_v3` (MessageEntity.cpp:3070-3238): text entities must be
/// valid; the reverse of [`parse_markdown_v3`].
pub fn get_markdown_v3(text: FormattedText) -> FormattedText {
    if text.entities.is_empty() {
        return text;
    }

    check_is_sorted(&text.entities);
    for entity in &text.entities {
        if !is_user_entity(entity_type(entity)) {
            return text;
        }
    }

    struct EntityInfo {
        index: usize,
        utf16_added_before: i32,
    }

    let is_valid_language_code =
        |code: &str| -> bool { code.bytes().all(|c| (33..=126).contains(&c)) };

    let mut result_text: Vec<u8> = Vec::new();
    let mut result_entities: Vec<MessageEntity> = Vec::new();
    let mut nested_entities_stack: Vec<EntityInfo> = Vec::new();
    let mut current_entity = 0usize;

    let mut utf16_offset = 0i32;
    let mut utf16_added = 0i32;

    let bytes = text.text.as_bytes();
    let mut pos = 0usize;
    while pos <= bytes.len() {
        // The C++ iterates pos <= size and reads `text.text[size]` as the
        // terminating '\0'; it is not a first code unit, so the final
        // iteration emits nothing and just breaks (no stack unwinding).
        let c = bytes.get(pos).copied().unwrap_or(0);
        if tdutils::utf8::is_utf8_character_first_code_unit(c) {
            while let Some(top) = nested_entities_stack.last() {
                let entity = &text.entities[top.index];
                let entity_end = entity.offset + entity.length;
                if utf16_offset < entity_end {
                    break;
                }

                assert_eq!(
                    utf16_offset, entity_end,
                    "the C++ CHECK(utf16_offset == entity_end)"
                );

                let mut need_entity = false;
                match entity_type(entity) {
                    Type::Italic => {
                        result_text.extend_from_slice(b"__");
                        utf16_added += 2;
                    }
                    Type::Bold => {
                        result_text.extend_from_slice(b"**");
                        utf16_added += 2;
                    }
                    Type::Strikethrough => {
                        result_text.extend_from_slice(b"~~");
                        utf16_added += 2;
                    }
                    Type::Spoiler => {
                        result_text.extend_from_slice(b"||");
                        utf16_added += 2;
                    }
                    Type::TextUrl => {
                        result_text.extend_from_slice(b"](");
                        result_text.extend_from_slice(entity.argument.as_bytes());
                        result_text.push(b')');
                        utf16_added += 3 + text_length(&entity.argument);
                    }
                    Type::Code => {
                        result_text.push(b'`');
                        utf16_added += 1;
                    }
                    Type::Pre => {
                        result_text.extend_from_slice(b"```");
                        utf16_added += 3;
                    }
                    Type::PreCode if is_valid_language_code(&entity.argument) => {
                        result_text.extend_from_slice(b"```");
                        utf16_added += 3;
                    }
                    Type::PreCode => {
                        need_entity = true;
                    }
                    _ => {
                        need_entity = true;
                    }
                }
                let utf16_added_before = top.utf16_added_before;
                if need_entity {
                    let mut entity = text.entities[top.index].clone();
                    entity.offset += utf16_added_before;
                    entity.length += utf16_added - utf16_added_before;
                    result_entities.push(entity);
                }
                nested_entities_stack.pop();
            }

            while current_entity < text.entities.len()
                && utf16_offset >= text.entities[current_entity].offset
            {
                assert_eq!(
                    utf16_offset, text.entities[current_entity].offset,
                    "the C++ CHECK(utf16_offset == offset)"
                );
                match entity_type(&text.entities[current_entity]) {
                    Type::Italic => {
                        result_text.extend_from_slice(b"__");
                        utf16_added += 2;
                    }
                    Type::Bold => {
                        result_text.extend_from_slice(b"**");
                        utf16_added += 2;
                    }
                    Type::Strikethrough => {
                        result_text.extend_from_slice(b"~~");
                        utf16_added += 2;
                    }
                    Type::Spoiler => {
                        result_text.extend_from_slice(b"||");
                        utf16_added += 2;
                    }
                    Type::TextUrl => {
                        result_text.push(b'[');
                        utf16_added += 1;
                    }
                    Type::Code => {
                        result_text.push(b'`');
                        utf16_added += 1;
                    }
                    Type::Pre => {
                        result_text.extend_from_slice(b"```");
                        utf16_added += 3;
                        if c != b'\n' {
                            result_text.push(b'\n');
                            utf16_added += 1;
                        }
                    }
                    Type::PreCode
                        if is_valid_language_code(&text.entities[current_entity].argument) =>
                    {
                        result_text.extend_from_slice(b"```");
                        utf16_added += 3;
                        if !text.entities[current_entity].argument.is_empty() {
                            let argument = text.entities[current_entity].argument.clone();
                            result_text.extend_from_slice(argument.as_bytes());
                            utf16_added += text_length(&argument);
                        }
                        if c != b'\n' {
                            result_text.push(b'\n');
                            utf16_added += 1;
                        }
                    }
                    // keep as is
                    _ => {}
                }
                nested_entities_stack.push(EntityInfo {
                    index: current_entity,
                    utf16_added_before: utf16_added,
                });
                current_entity += 1;
            }
            utf16_offset += 1 + i32::from(c >= 0xf0); // >= 4 bytes in symbol => surrogate pair
        }
        if pos == bytes.len() {
            break;
        }

        result_text.push(c);

        pos += 1;
    }

    let mut result = FormattedText {
        text: String::from_utf8(result_text).expect("the text is a copy of valid UTF-8 input"),
        entities: result_entities,
    };
    sort_entities(&mut result.entities);
    if parse_markdown_v3(result.clone()) != text {
        return text;
    }
    result
}

// ===== M.8.9 — HTML (MessageEntity.cpp:3240-3577) =====

/// `td::is_alpha` (misc.h:111-114).
fn is_alpha(c: u8) -> bool {
    c.is_ascii_alphabetic()
}

/// `td::is_digit` (misc.h) — ASCII digits only.
fn is_digit(c: u8) -> bool {
    c.is_ascii_digit()
}

/// `td::is_alnum` (misc.h).
fn is_alnum(c: u8) -> bool {
    is_alpha(c) || is_digit(c)
}

/// `td::is_hex_digit` (misc.h).
fn is_hex_digit(c: u8) -> bool {
    c.is_ascii_hexdigit()
}

/// `td::hex_to_int` (misc.h): the value of a hex digit.
fn hex_to_int(c: u8) -> u32 {
    let c = c | 0x20;
    if c.is_ascii_digit() {
        u32::from(c - b'0')
    } else {
        u32::from(c - b'a' + 10)
    }
}

/// `td::to_integer<int32>` (misc.h:157-182): the wrapping leading-digits
/// parse with the C++ two's-complement overflow dance.
fn to_integer_i32(s: &str) -> i32 {
    let bytes = s.as_bytes();
    let mut integer_value: u32 = 0;
    let mut begin = 0usize;
    let mut is_negative = false;
    if begin < bytes.len() && bytes[begin] == b'-' {
        is_negative = true;
        begin += 1;
    }
    while begin < bytes.len() && is_digit(bytes[begin]) {
        integer_value = integer_value
            .wrapping_mul(10)
            .wrapping_add(u32::from(bytes[begin] - b'0'));
        begin += 1;
    }
    if integer_value > i32::MAX as u32 {
        // Use ~x + 1 instead of -x to suppress Visual Studio warning.
        integer_value = (!integer_value).wrapping_add(1);
        is_negative = !is_negative;

        if integer_value > i32::MAX as u32 {
            return i32::MIN;
        }
    }

    if is_negative {
        -(integer_value as i32)
    } else {
        integer_value as i32
    }
}

/// `decode_html_entity` (MessageEntity.cpp:3240-3287): decodes the entity
/// at `pos` (which must point at '&') and advances `pos` past it. Zero
/// means the entity was not recognized.
fn decode_html_entity(bytes: &[u8], pos: &mut usize) -> u32 {
    assert_eq!(bytes[*pos], b'&', "the C++ CHECK(text[pos] == '&')");
    let at = |i: usize| -> u8 { bytes.get(i).copied().unwrap_or(0) };
    let mut end_pos = *pos + 1;
    let mut res = 0u32;
    if at(*pos + 1) == b'#' {
        // numeric character reference
        end_pos += 1;
        if at(*pos + 2) == b'x' {
            // hexadecimal numeric character reference
            end_pos += 1;
            while is_hex_digit(at(end_pos)) {
                // The C++ accumulates into a uint32; the value only matters
                // when the span check below accepts it, but the multiplication
                // must not panic in debug builds on the rejected long forms.
                res = res.wrapping_mul(16).wrapping_add(hex_to_int(at(end_pos)));
                end_pos += 1;
            }
        } else {
            // decimal numeric character reference
            while is_digit(at(end_pos)) {
                res = res
                    .wrapping_mul(10)
                    .wrapping_add(u32::from(at(end_pos) - b'0'));
                end_pos += 1;
            }
        }
        if res == 0 || res >= 0x10ffff || end_pos - *pos >= 10 {
            return 0;
        }
    } else {
        while is_alpha(at(end_pos)) {
            end_pos += 1;
        }
        let entity = &bytes[*pos + 1..end_pos.min(bytes.len())];
        match entity {
            b"lt" => res = u32::from(b'<'),
            b"gt" => res = u32::from(b'>'),
            b"amp" => res = u32::from(b'&'),
            b"quot" => res = u32::from(b'"'),
            // unsupported literal entity
            _ => return 0,
        }
    }

    if at(end_pos) == b';' {
        *pos = end_pos + 1;
    } else {
        *pos = end_pos;
    }
    res
}

/// `parse_html` (MessageEntity.cpp:3289-3577). The C++ decodes quoted
/// attribute values in place and compacts `str` in place; the port decodes
/// into fresh buffers and rewrites `str` at the end.
pub fn parse_html(str: &mut String) -> Result<Vec<MessageEntity>, Status> {
    let bytes = str.as_bytes();
    // The C++ reads past the end of the NUL-terminated string; those reads
    // yield '\0'.
    let at = |i: usize| -> u8 { bytes.get(i).copied().unwrap_or(0) };
    let str_size = bytes.len();

    let mut result: Vec<u8> = Vec::with_capacity(str_size);
    let mut entities: Vec<MessageEntity> = Vec::new();
    let mut utf16_offset = 0i32;
    let mut need_recheck_utf8 = false;

    struct EntityInfo {
        tag_name: String,
        argument: String,
        date: i32,
        entity_offset: i32,
        entity_begin_pos: usize,
    }
    let mut nested_entities: Vec<EntityInfo> = Vec::new();

    let mut i = 0usize;
    while i < str_size {
        let c = at(i);
        if c == b'&' {
            let mut pos = i;
            let code = decode_html_entity(bytes, &mut pos);
            if code != 0 {
                i = pos - 1; // i will be incremented below
                utf16_offset += 1 + i32::from(code > 0xffff);
                if (0xd800..=0xdfff).contains(&code) {
                    // half of a surrogate pair
                    need_recheck_utf8 = true;
                }
                tdutils::utf8::append_utf8_character(&mut result, code);
                assert!(
                    result.len() <= i,
                    "the C++ CHECK(result_end <= result_begin + i)"
                );
                i += 1;
                continue;
            }
        }
        if c != b'<' {
            if tdutils::utf8::is_utf8_character_first_code_unit(c) {
                utf16_offset += 1 + i32::from(c >= 0xf0); // >= 4 bytes in symbol => surrogate pair
            }
            result.push(c);
            i += 1;
            continue;
        }

        let begin_pos = i;
        i += 1;
        if at(i) != b'/' {
            // begin of an entity
            while !is_space(at(i)) && at(i) != b'>' {
                i += 1;
            }
            if at(i) == 0 {
                return Err(Status::error(
                    400,
                    &format!("Unclosed start tag at byte offset {}", begin_pos),
                ));
            }

            let tag_name = str[begin_pos + 1..i].to_ascii_lowercase();
            if !matches!(
                tag_name.as_str(),
                "a" | "b"
                    | "strong"
                    | "i"
                    | "em"
                    | "s"
                    | "strike"
                    | "del"
                    | "u"
                    | "ins"
                    | "tg-spoiler"
                    | "tg-emoji"
                    | "tg-time"
                    | "span"
                    | "pre"
                    | "code"
                    | "blockquote"
            ) {
                return Err(Status::error(
                    400,
                    &format!(
                        "Unsupported start tag \"{}\" at byte offset {}",
                        tag_name, begin_pos
                    ),
                ));
            }

            let mut argument = String::new();
            let mut date = 0i32;
            while at(i) != b'>' {
                while at(i) != 0 && is_space(at(i)) {
                    i += 1;
                }
                if at(i) == b'>' {
                    break;
                }
                let attribute_begin_pos = i;
                while !is_space(at(i))
                    && at(i) != b'='
                    && at(i) != b'>'
                    && at(i) != b'/'
                    && at(i) != b'"'
                    && at(i) != b'\''
                {
                    i += 1;
                }
                let attribute_name = &str[attribute_begin_pos..i];
                if attribute_name.is_empty() {
                    return Err(Status::error(
                        400,
                        &format!(
                            "Empty attribute name in the tag \"{}\" at byte offset {}",
                            tag_name, begin_pos
                        ),
                    ));
                }
                while at(i) != 0 && is_space(at(i)) {
                    i += 1;
                }
                if at(i) != b'=' {
                    if at(i) == 0 {
                        return Err(Status::error(
                            400,
                            &format!(
                                "Unclosed start tag \"{}\" at byte offset {}",
                                tag_name, begin_pos
                            ),
                        ));
                    }
                    if tag_name == "blockquote" && attribute_name == "expandable" {
                        argument = "1".to_owned();
                    }
                    continue;
                }
                i += 1;
                while at(i) != 0 && is_space(at(i)) {
                    i += 1;
                }
                if at(i) == 0 {
                    return Err(Status::error(
                        400,
                        &format!(
                            "Unclosed start tag \"{}\" at byte offset {}",
                            tag_name, begin_pos
                        ),
                    ));
                }

                let attribute_value: String;
                if at(i) != b'\'' && at(i) != b'"' {
                    // A name token (a sequence of letters, digits, periods, or hyphens). Name tokens are not case sensitive.
                    let token_begin_pos = i;
                    while is_alnum(at(i)) || at(i) == b'.' || at(i) == b'-' {
                        i += 1;
                    }
                    attribute_value = str[token_begin_pos..i].to_ascii_lowercase();

                    if !is_space(at(i)) && at(i) != b'>' {
                        return Err(Status::error(
                            400,
                            &format!(
                                "Unexpected end of name token at byte offset {}",
                                token_begin_pos
                            ),
                        ));
                    }
                } else {
                    // A string literal; the C++ decodes it in place, the
                    // port decodes into a fresh buffer.
                    let end_character = at(i);
                    i += 1;
                    let mut decoded: Vec<u8> = Vec::new();
                    while at(i) != end_character && at(i) != 0 {
                        if at(i) == b'&' {
                            let mut pos = i;
                            let code = decode_html_entity(bytes, &mut pos);
                            if code != 0 {
                                i = pos;
                                tdutils::utf8::append_utf8_character(&mut decoded, code);
                                continue;
                            }
                        }
                        decoded.push(at(i));
                        i += 1;
                    }
                    if at(i) == end_character {
                        i += 1;
                    }
                    // The C++ keeps the decoded bytes in a std::string, where
                    // surrogate halves encoded by numeric entities stay as
                    // raw bytes; Rust strings cannot hold those, so they are
                    // replaced here. Every consumer either byte-compares
                    // against ASCII (span/blockquote), parses leading digits
                    // (tg-time unix) or rejects non-ASCII wholesale (links),
                    // so the C++ outcome is preserved.
                    attribute_value = String::from_utf8_lossy(&decoded).into_owned();
                }
                if at(i) == 0 {
                    return Err(Status::error(
                        400,
                        &format!("Unclosed start tag at byte offset {}", begin_pos),
                    ));
                }

                if tag_name == "a" && attribute_name == "href" {
                    argument = attribute_value;
                } else if tag_name == "code"
                    && attribute_name == "class"
                    && attribute_value.starts_with("language-")
                {
                    argument = attribute_value[9..].to_owned();
                } else if tag_name == "span"
                    && attribute_name == "class"
                    && attribute_value.starts_with("tg-")
                {
                    argument = attribute_value[3..].to_owned();
                } else if tag_name == "tg-emoji" && attribute_name == "emoji-id" {
                    argument = attribute_value;
                } else if tag_name == "blockquote" && attribute_name == "expandable" {
                    argument = "1".to_owned();
                } else if tag_name == "tg-time" && attribute_name == "unix" {
                    date = to_integer_i32(&attribute_value);
                } else if tag_name == "tg-time" && attribute_name == "format" {
                    argument = attribute_value;
                }
            }

            if tag_name == "span" && argument != "spoiler" {
                return Err(Status::error(
                    400,
                    &format!(
                        "Tag \"span\" must have class \"tg-spoiler\" at byte offset {}",
                        begin_pos
                    ),
                ));
            }

            nested_entities.push(EntityInfo {
                tag_name,
                argument,
                date,
                entity_offset: utf16_offset,
                entity_begin_pos: result.len(),
            });
        } else {
            // end of an entity
            if nested_entities.is_empty() {
                return Err(Status::error(
                    400,
                    &format!("Unexpected end tag at byte offset {}", begin_pos),
                ));
            }

            while !is_space(at(i)) && at(i) != b'>' {
                i += 1;
            }
            let end_tag_name = if begin_pos + 2 <= i {
                str[begin_pos + 2..i].to_ascii_lowercase()
            } else {
                String::new()
            };
            while is_space(at(i)) && at(i) != 0 {
                i += 1;
            }
            if at(i) != b'>' {
                return Err(Status::error(
                    400,
                    &format!("Unclosed end tag at byte offset {}", begin_pos),
                ));
            }

            let tag_name = nested_entities.last().unwrap().tag_name.clone();
            if !end_tag_name.is_empty() && end_tag_name != tag_name {
                return Err(Status::error(
                    400,
                    &format!(
                        "Unmatched end tag at byte offset {}, expected \"</{}>\", found \"</{}>\"",
                        begin_pos, tag_name, end_tag_name
                    ),
                ));
            }

            if utf16_offset > nested_entities.last().unwrap().entity_offset {
                let entity_offset = nested_entities.last().unwrap().entity_offset;
                let entity_length = utf16_offset - entity_offset;
                let back_argument = nested_entities.last().unwrap().argument.clone();
                let back_date = nested_entities.last().unwrap().date;
                let back_entity_begin_pos = nested_entities.last().unwrap().entity_begin_pos;
                if tag_name == "i" || tag_name == "em" {
                    entities.push(MessageEntity::new(
                        Type::Italic,
                        entity_offset,
                        entity_length,
                    ));
                } else if tag_name == "b" || tag_name == "strong" {
                    entities.push(MessageEntity::new(Type::Bold, entity_offset, entity_length));
                } else if tag_name == "s" || tag_name == "strike" || tag_name == "del" {
                    entities.push(MessageEntity::new(
                        Type::Strikethrough,
                        entity_offset,
                        entity_length,
                    ));
                } else if tag_name == "u" || tag_name == "ins" {
                    entities.push(MessageEntity::new(
                        Type::Underline,
                        entity_offset,
                        entity_length,
                    ));
                } else if tag_name == "tg-spoiler"
                    || (tag_name == "span" && back_argument == "spoiler")
                {
                    entities.push(MessageEntity::new(
                        Type::Spoiler,
                        entity_offset,
                        entity_length,
                    ));
                } else if tag_name == "tg-emoji" {
                    let Some(document_id) =
                        crate::config_manager::to_integer_safe_i64(&back_argument)
                    else {
                        return Err(Status::error(
                            400,
                            "Invalid custom emoji identifier specified",
                        ));
                    };
                    if document_id == 0 {
                        return Err(Status::error(
                            400,
                            "Invalid custom emoji identifier specified",
                        ));
                    }
                    entities.push(MessageEntity::custom_emoji(
                        entity_offset,
                        entity_length,
                        CustomEmojiId::new(document_id),
                    ));
                } else if tag_name == "tg-time" {
                    let formatted_date =
                        FormattedDate::get_formatted_date_by_format(back_date, &back_argument)?;
                    if back_date > 0 {
                        entities.push(MessageEntity::formatted_date(
                            entity_offset,
                            entity_length,
                            formatted_date,
                        ));
                    }
                } else if tag_name == "a" {
                    let mut url = std::mem::take(&mut nested_entities.last_mut().unwrap().argument);
                    if url.is_empty() {
                        // Surrogate halves from decoded entities become
                        // U+FFFD; the C++ passes the raw bytes to the link
                        // checkers, which reject non-ASCII either way, and
                        // the final check_utf8 still fails on surrogates.
                        url =
                            String::from_utf8_lossy(&result[back_entity_begin_pos..]).into_owned();
                    }
                    let user_id = crate::link_manager::get_link_user_id(&url);
                    if user_id.is_valid() {
                        entities.push(MessageEntity::mention_name(
                            entity_offset,
                            entity_length,
                            user_id,
                        ));
                    } else {
                        let url = crate::link_manager::get_checked_link(&url);
                        if !url.is_empty() {
                            entities.push(MessageEntity::with_argument(
                                Type::TextUrl,
                                entity_offset,
                                entity_length,
                                url,
                            ));
                        }
                    }
                } else if tag_name == "pre" {
                    let is_code_with_language = matches!(
                        entities.last(),
                        Some(entity)
                            if entity.r#type == Some(Type::Code)
                                && entity.offset == entity_offset
                                && entity.length == entity_length
                                && !entity.argument.is_empty()
                    );
                    if is_code_with_language {
                        let last = entities.last_mut().unwrap();
                        last.r#type = Some(Type::PreCode);
                    } else {
                        entities.push(MessageEntity::new(Type::Pre, entity_offset, entity_length));
                    }
                } else if tag_name == "code" {
                    let is_pre_with_language = matches!(
                        entities.last(),
                        Some(entity)
                            if entity.r#type == Some(Type::Pre)
                                && entity.offset == entity_offset
                                && entity.length == entity_length
                    );
                    if is_pre_with_language && !back_argument.is_empty() {
                        let last = entities.last_mut().unwrap();
                        last.r#type = Some(Type::PreCode);
                        last.argument = back_argument;
                    } else {
                        entities.push(MessageEntity::with_argument(
                            Type::Code,
                            entity_offset,
                            entity_length,
                            back_argument,
                        ));
                    }
                } else if tag_name == "blockquote" {
                    if !back_argument.is_empty() {
                        entities.push(MessageEntity::new(
                            Type::ExpandableBlockQuote,
                            entity_offset,
                            entity_length,
                        ));
                    } else {
                        entities.push(MessageEntity::new(
                            Type::BlockQuote,
                            entity_offset,
                            entity_length,
                        ));
                    }
                } else {
                    unreachable!("the C++ UNREACHABLE()");
                }
            }
            nested_entities.pop();
        }
        i += 1;
    }
    if let Some(back) = nested_entities.last() {
        return Err(Status::error(
            400,
            &format!(
                "Can't find end tag corresponding to start tag \"{}\"",
                back.tag_name
            ),
        ));
    }

    for entity in entities.iter_mut() {
        if entity.r#type == Some(Type::Code) && !entity.argument.is_empty() {
            entity.argument.clear();
        }
    }

    sort_entities(&mut entities);

    // The C++ checks the resized std::string, which can hold surrogate
    // halves; Rust strings cannot, so the check runs on the byte buffer
    // before the conversion.
    if need_recheck_utf8 && !tdutils::utf8::check_utf8(&result) {
        return Err(Status::error(
            400,
            "Text contains invalid Unicode characters after decoding HTML entities, check for unmatched surrogate code units",
        ));
    }
    *str = String::from_utf8(result).expect("the compaction keeps whole characters");
    Ok(entities)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formatted_date::date_flags;

    fn json(value: &impl serde::Serialize) -> String {
        serde_json::to_string(value).unwrap()
    }

    fn entity(t: Type, offset: i32, length: i32) -> MessageEntity {
        MessageEntity::new(t, offset, length)
    }

    #[test]
    fn type_priority_table() {
        // MessageEntity.cpp:38-64 — the full 23-entry table, in C++ order.
        const PRIORITIES: [(Type, i32); Type::SIZE as usize] = [
            (Type::Mention, 50),
            (Type::Hashtag, 50),
            (Type::BotCommand, 50),
            (Type::Url, 50),
            (Type::EmailAddress, 50),
            (Type::Bold, 90),
            (Type::Italic, 91),
            (Type::Code, 20),
            (Type::Pre, 11),
            (Type::PreCode, 10),
            (Type::TextUrl, 49),
            (Type::MentionName, 49),
            (Type::Cashtag, 50),
            (Type::PhoneNumber, 50),
            (Type::Underline, 92),
            (Type::Strikethrough, 93),
            (Type::BlockQuote, 0),
            (Type::BankCardNumber, 50),
            (Type::MediaTimestamp, 50),
            (Type::Spoiler, 94),
            (Type::CustomEmoji, 99),
            (Type::ExpandableBlockQuote, 0),
            (Type::FormattedDate, 30),
        ];
        for (r#type, priority) in PRIORITIES {
            assert_eq!(MessageEntity::type_priority(r#type), priority, "{type:?}");
        }
    }

    /// The M.8.3 matcher battery — the exact inputs whose C++ `td_execute`
    /// `getTextEntities` outputs are recorded as the 21 entries of
    /// `testdata/message_entity_cpp_golden.jsonl` right before the M.8.4
    /// URL battery.
    fn matcher_battery() -> Vec<String> {
        let mut battery: Vec<String> = [
            // mentions
            "@ab @abc @gif @durov @a @verylongusername1234567890123456",
            "@abcdefghijklmnopqrstuvwxyz012345 @durov_2 @durov@mention",
            "(@durov) @gif. @ufc- @vid, @nft! @pic?",
            "x@durov @user\u{e9} а@bot @@gif @",
            // bot commands
            "/start /start@my_bot /a /start@b /a_b /123 /",
            "a/start /start/next </start> >/cmd",
            "",
            "/start@bot_name@x /help@my_bot_2",
            // hashtags
            "#tag #tag2 #123 #a_b #tag- #tag#x ##a #ТГ #привет_2 #tag\u{200c}x",
            "#tag@botname #tag@b #x#y",
            "",
            // cashtags
            "$USD $usd $1INCH $AB $ABCDEFGHI $USD@bot $USD@ $USDx $$USD x$USD $5 $9A",
            "$TON $NOTCOIN $1inch",
            // media timestamps
            "1:23 12:34:56 0:00 9999:59 12:60 1:2 1:234",
            "a1:23 1:23b :23 1: 1::23 00:00:00 100:00:00 99:99:99 1:23:45:67",
            // bank cards
            "4242424242424242 4242 4242 4242 4242 4242-4242-4242-4242 1234567812345678",
            "5555555555554444 378282246310005 6212345678901232 2201382000000013 4222222222222",
            "x4242424242424242 4242424242424242x tel:4242424242424242 +1-4242-4242-4242-4242",
            "4242424242424242 123 4 2 4 2 4 2 4 2 4 2 4 2 4 2 4 2",
            // emails (is_email_address feeds the EmailAddress entities)
            "a@b.co a@b.c a.b+c-d@sub.domain.example.org x@y.z user@sub_domain.example.com user@-sub.example.com",
            // combined matcher line (no URLs)
            "@durov #tag $USD /cmd 1:23 4242424242424242",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        battery[6] = format!("/{} /{}", "a".repeat(64), "a".repeat(65));
        battery[10] = format!("#{} #{}", "a".repeat(256), "a".repeat(257));
        battery
    }

    /// M.8.3 property test: every extraction of every matcher over the
    /// battery is a substring of the input, extractions are ordered and
    /// non-overlapping, and the structural shape of each family holds.
    #[test]
    fn matcher_extractions_are_substrings_of_the_input() {
        fn check(text: &str, extractions: &[&str]) {
            let mut last_end = 0usize;
            for &extraction in extractions {
                let pos = text[last_end..]
                    .find(extraction)
                    .map(|pos| pos + last_end)
                    .unwrap_or_else(|| {
                        panic!("extraction {extraction:?} is not a substring of {text:?}")
                    });
                assert!(
                    pos >= last_end,
                    "extractions overlap or go backwards in {text:?}"
                );
                last_end = pos + extraction.len();
            }
        }

        for text in matcher_battery() {
            check(&text, &match_mentions(&text));
            check(&text, &match_bot_commands(&text));
            check(&text, &match_hashtags(&text));
            check(&text, &match_cashtags(&text));
            check(&text, &match_media_timestamps(&text));
            check(&text, &match_bank_card_numbers(&text));

            let mentions = find_mentions(&text);
            check(&text, &mentions);
            assert!(mentions.iter().all(|m| m.starts_with('@')));

            let commands = find_bot_commands(&text);
            check(&text, &commands);
            assert!(commands.iter().all(|command| command.starts_with('/')));

            let hashtags = find_hashtags(&text);
            check(&text, &hashtags);
            assert!(hashtags.iter().all(|hashtag| hashtag.starts_with('#')));

            let cashtags = find_cashtags(&text);
            check(&text, &cashtags);
            assert!(cashtags.iter().all(|cashtag| cashtag.starts_with('$')));

            let media_timestamps = find_media_timestamps(&text);
            let slices: Vec<&str> = media_timestamps.iter().map(|&(slice, _)| slice).collect();
            check(&text, &slices);
            assert!(media_timestamps.iter().all(|&(slice, _)| {
                !slice.is_empty()
                    && slice.contains(':')
                    && slice.chars().all(|c| c.is_ascii_digit() || c == ':')
            }));

            let bank_cards = find_bank_card_numbers(&text);
            check(&text, &bank_cards);
            assert!(bank_cards.iter().all(|bank_card| {
                bank_card
                    .bytes()
                    .all(|b| b.is_ascii_digit() || b == b' ' || b == b'-')
                    && bank_card.bytes().filter(|b| b.is_ascii_digit()).count() >= 13
                    && is_valid_bank_card(bank_card)
            }));
        }
    }

    /// M.8.3 golden vectors: extraction lists hand-checked against the C++
    /// matcher semantics; they are byte-identical to the recorded C++
    /// `getTextEntities` outputs for the same inputs
    /// (`testdata/message_entity_cpp_golden.jsonl`).
    #[test]
    fn matcher_extraction_golden_vectors() {
        assert_eq!(
            find_mentions("@ab @abc @gif @durov @a @verylongusername1234567890123456"),
            ["@gif", "@durov", "@verylongusername1234567890123456"]
        );
        assert_eq!(
            find_mentions("(@durov) @gif. @ufc- @vid, @nft! @pic?"),
            ["@durov", "@gif", "@ufc", "@vid", "@nft", "@pic"]
        );
        assert_eq!(
            find_bot_commands("/start /start@my_bot /a /start@b /a_b /123 /"),
            ["/start", "/start@my_bot", "/a", "/a_b", "/123"]
        );
        assert_eq!(
            find_hashtags("#tag #tag2 #123 #a_b #tag- #tag#x ##a #ТГ #привет_2 #tag\u{200c}x"),
            [
                "#tag",
                "#tag2",
                "#a_b",
                "#tag",
                "#a",
                "#ТГ",
                "#привет_2",
                "#tag\u{200c}x"
            ]
        );
        assert_eq!(
            find_cashtags(
                "$USD $usd $1INCH $AB $ABCDEFGHI $USD@bot $USD@ $USDx $$USD x$USD $5 $9A"
            ),
            ["$USD", "$1INCH", "$AB", "$USD@bot", "$USD"]
        );
        assert_eq!(
            find_media_timestamps("1:23 12:34:56 0:00 9999:59 12:60 1:2 1:234"),
            [
                ("1:23", 83),
                ("12:34:56", 45296),
                ("0:00", 0),
                ("9999:59", 599999)
            ]
        );
        assert_eq!(
            find_media_timestamps(
                "a1:23 1:23b :23 1: 1::23 00:00:00 100:00:00 99:99:99 1:23:45:67"
            ),
            [("00:00:00", 0)]
        );
        // The card after the first one is consumed by the 32-digit run the
        // scanner keeps merging, so only the first card survives.
        assert_eq!(
            find_bank_card_numbers(
                "4242424242424242 4242 4242 4242 4242 4242-4242-4242-4242 1234567812345678"
            ),
            ["4242424242424242"]
        );
        assert_eq!(
            find_bank_card_numbers(
                "5555555555554444 378282246310005 6212345678901232 2201382000000013 4222222222222"
            ),
            ["5555555555554444"]
        );
        // The match_* layer keeps candidates the find_* layer drops: short
        // non-reserved usernames are removed by find_mentions only.
        assert_eq!(match_mentions("@abc"), ["@abc"]);
        assert_eq!(find_mentions("@abc"), Vec::<&str>::new());
        // A media timestamp directly before a card: the seconds digits join
        // the card scan run, the merged run fails Luhn, nothing is found.
        assert_eq!(
            find_media_timestamps("12:34 4242424242424242"),
            [("12:34", 754)]
        );
        assert_eq!(
            find_bank_card_numbers("12:34 4242424242424242"),
            Vec::<&str>::new()
        );
        assert_eq!(
            find_bank_card_numbers("4242424242424242 1:23"),
            ["4242424242424242"]
        );
    }

    /// The M.8.4 URL battery — the exact inputs whose C++ `td_execute`
    /// `getTextEntities` outputs are recorded as the last 41 entries of
    /// `testdata/message_entity_cpp_golden.jsonl`: Unicode URLs, trailing
    /// punctuation, IDN-lite paths, tg:// links, ports, protocols, user
    /// data, IPv4, typo-squats, and the byte-prefix regressions.
    fn url_battery() -> Vec<String> {
        vec![
            "мюнхен.рф и москва.рф/путь?q=1 и сайт.онлайн".to_string(),
            "123456789ü.de and 12345678€.de".to_string(),
            "日本語.jp/パス/次 and https://例え.テスト/".to_string(),
            "ünü.de and bücher.eu/test/ü".to_string(),
            "Visit example.com, (or example.org/test), check test.net! bye.".to_string(),
            "go to example.com/path?q=1&x=2, then example.com/word.".to_string(),
            "example.com/a(b)c(d) and example.com/a(b — balance".to_string(),
            "example.com/foo):bar and example.com/foo):".to_string(),
            "example.com/(foo)! example.com/(unclosed example.com/a,b,c".to_string(),
            "example.com/a<b>c example.com/a«b» example.com/a\"b example.com/a'b".to_string(),
            "example.com/a\nb and example.com/a\u{200c}b and example.com/a\u{200b}b".to_string(),
            "xn--bcher-kva.de/start and xn--80adxhks.xn--p1ai and xn--bcher-kva.notatld"
                .to_string(),
            "münchen.xn--bcher-kva and xn-- and xn--abc.com".to_string(),
            "tg://resolve?domain=telegram and TG://emoji?id=5 and ton://transfer/x".to_string(),
            "tonsite://deploy.ton and tonsite://x and tonsite://".to_string(),
            "tg://дуров and tg:// and t.me and t.me/durov".to_string(),
            "tg://x? and tg://xxtg://y".to_string(),
            "example.com:8080/x example.com:65535 example.com:65536 example.com:99999 example.com:0"
                .to_string(),
            "http://x.com HTTPS://X.COM ftp://files.example.org tftp://x.com sftp://x.com"
                .to_string(),
            "seehttps://x.com xhttp://y.com mysftp://x.com FTP://X.COM".to_string(),
            "mailto:user@example.com user:pass@example.com user@example.com".to_string(),
            "127.0.0.1/x 256.256.256.256 0.0.0.0 255.255.255.255 1.2.3.4 999.1.2.3 025.1.2.3 25.1.2.3"
                .to_string(),
            "12.34.56.78 1234.5678.9012.3456 1.2.3.4.5 3.14".to_string(),
            "example..com example.com- example-.com example. unknown.example-tld".to_string(),
            "http://no-tld.xyzabc no-tld.xyzabc https://a.b.c".to_string(),
            "teiegram.org xteiegram.org TEIEGRAM.ORG".to_string(),
            "a.b a.c e.c e.co ab.cd com t.me".to_string(),
            "exa_mple.com ex-ample.com word.word example.kaufen".to_string(),
            "example.com. example.com..next example.com#frag example.com/#".to_string(),
            "example.com/! example.com/? example.com/?a=b".to_string(),
            "https://t.me/durov and t.me @durov #tag https://example.com".to_string(),
            "http://.mee http://./ http://.com/ https://.".to_string(),
            "example.com/путь/дальше and example.com/→".to_string(),
            "https://example.com/path/ü and https://ün.de/x".to_string(),
            "www.example.com https://www.example.com/path www.example.com:443".to_string(),
            "meet.google.com/abc-def and a.b c.de 3.14 and 2.5".to_string(),
            "tonsite://deploy.ton/tgz and t.me/s/mode".to_string(),
            "See (https://example.com/a) and [example.com/b] {example.com/c}".to_string(),
            "example.com/вопрос) example.com/ув!".to_string(),
            "tg://emoji?id=5😀 and (@example.com)".to_string(),
            "h.t.t.p://x.com http:://x.com —https://x.com".to_string(),
        ]
    }

    /// M.8.4 property test: every URL extraction over the battery is a
    /// substring of the input, extractions are ordered and non-overlapping,
    /// and each family keeps its structural shape.
    #[test]
    fn url_extractions_are_substrings_of_the_input() {
        fn check(text: &str, extractions: &[&str]) {
            let mut last_end = 0usize;
            for &extraction in extractions {
                let pos = text[last_end..]
                    .find(extraction)
                    .map(|pos| pos + last_end)
                    .unwrap_or_else(|| {
                        panic!("extraction {extraction:?} is not a substring of {text:?}")
                    });
                assert!(
                    pos >= last_end,
                    "extractions overlap or go backwards in {text:?}"
                );
                last_end = pos + extraction.len();
            }
        }

        for text in url_battery() {
            check(&text, &match_tg_urls(&text));
            check(&text, &find_tg_urls(&text));

            let urls: Vec<&str> = find_urls(&text).into_iter().map(|(url, _)| url).collect();
            check(&text, &urls);
            assert!(
                urls.iter()
                    .all(|url| !url.is_empty() && (url.contains('.') || url.contains('@')))
            );

            let tg_urls = find_tg_urls(&text);
            assert!(tg_urls.iter().all(|url| url.contains("://")));
        }
    }

    /// M.8.4 golden vectors: extraction lists hand-checked against the
    /// recorded C++ `getTextEntities` outputs for the same inputs
    /// (`testdata/message_entity_cpp_golden.jsonl`).
    #[test]
    fn url_extraction_golden_vectors() {
        // The C++ tonsite quirk, preserved verbatim: the tg-url matcher
        // slices from `ptr - 3`, so "tonsite://" matches begin at "ite".
        assert_eq!(
            find_tg_urls("tonsite://deploy.ton and tonsite://x and tonsite://"),
            ["ite://deploy", "ite://x"]
        );
        assert_eq!(
            find_tg_urls("tg://resolve?domain=telegram and TG://emoji?id=5 and ton://transfer/x"),
            [
                "tg://resolve?domain=telegram",
                "TG://emoji?id=5",
                "ton://transfer/x"
            ]
        );
        // The trailing "://y" is unreachable: the tg-url loop requires
        // strictly more than 5 bytes left at the second ':'.
        assert_eq!(
            find_tg_urls("tg://x? and tg://xxtg://y"),
            ["tg://x", "tg://xxtg"]
        );
        // tg:// links with a non-ASCII domain never match.
        assert_eq!(
            find_tg_urls("tg://дуров and tg:// and t.me and t.me/durov"),
            Vec::<&str>::new()
        );

        // IDN domains with Cyrillic TLDs.
        assert_eq!(
            find_urls("мюнхен.рф и москва.рф/путь?q=1 и сайт.онлайн"),
            [
                ("мюнхен.рф", false),
                ("москва.рф/путь?q=1", false),
                ("сайт.онлайн", false)
            ]
        );
        // Multibyte code points split at the 10-byte protocol probe — the
        // byte-prefix regression inputs.
        assert_eq!(
            find_urls("123456789ü.de and 12345678€.de"),
            [("123456789ü.de", false), ("12345678€.de", false)]
        );
        assert_eq!(
            find_urls("日本語.jp/パス/次 and https://例え.テスト/"),
            [
                ("日本語.jp/パス/次", false),
                ("https://例え.テスト/", false)
            ]
        );
        // The path ends at the unbalanced ')' and the closing chars drop.
        assert_eq!(
            find_urls("example.com/foo):bar and example.com/foo):"),
            [("example.com/foo", false), ("example.com/foo", false)]
        );
        // The mailto: prefix becomes an EmailAddress; a bare user@host is
        // an EmailAddress, but user:pass@host stays a URL.
        assert_eq!(
            find_urls("mailto:user@example.com user:pass@example.com user@example.com"),
            [
                ("user@example.com", true),
                ("user:pass@example.com", false),
                ("user@example.com", true)
            ]
        );
        // IPv4 and short numeric domains; >255 octets and leading zeros
        // are rejected.
        assert_eq!(
            find_urls(
                "127.0.0.1/x 256.256.256.256 0.0.0.0 255.255.255.255 1.2.3.4 999.1.2.3 025.1.2.3 25.1.2.3"
            ),
            [
                ("127.0.0.1/x", false),
                ("0.0.0.0", false),
                ("255.255.255.255", false),
                ("1.2.3.4", false),
                ("25.1.2.3", false)
            ]
        );
        assert_eq!(
            find_urls("example..com example.com- example-.com example. unknown.example-tld"),
            Vec::<(&str, bool)>::new()
        );
        // A protocol forgives an unknown TLD, but a one-code-point TLD is
        // always rejected.
        assert_eq!(
            find_urls("http://no-tld.xyzabc no-tld.xyzabc https://a.b.c"),
            [("http://no-tld.xyzabc", false)]
        );
        // The teiegram.org typo-squat is rejected, but only at exactly
        // 12 bytes.
        assert_eq!(
            find_urls("teiegram.org xteiegram.org TEIEGRAM.ORG"),
            [("xteiegram.org", false)]
        );
        // '_' in a domain rejects it; '-' only rejects when it ends a
        // domain part.
        assert_eq!(
            find_urls("exa_mple.com ex-ample.com word.word example.kaufen"),
            [("ex-ample.com", false), ("example.kaufen", false)]
        );
        assert_eq!(
            find_urls("example.com/! example.com/? example.com/?a=b"),
            [
                ("example.com/", false),
                ("example.com/", false),
                ("example.com/?a=b", false)
            ]
        );
        assert_eq!(
            find_urls("http://.mee http://./ http://.com/ https://."),
            Vec::<(&str, bool)>::new()
        );
        // Protocol candidates with extra scheme symbols are cut to the
        // last known scheme.
        assert_eq!(
            find_urls("h.t.t.p://x.com http:://x.com —https://x.com"),
            [("https://x.com", false)]
        );
        assert_eq!(
            find_urls("meet.google.com/abc-def and a.b c.de 3.14 and 2.5"),
            [("meet.google.com/abc-def", false), ("c.de", false)]
        );
        // Only the URL matcher, not the tg-url matcher, runs fix_url.
        assert_eq!(
            find_urls("tonsite://deploy.ton and tonsite://x and tonsite://"),
            [("tonsite://deploy.ton", false)]
        );
    }

    /// M.8.4 regression: the protocol probes read the first 10 (`fix_url`)
    /// or 8 (`get_first_url`) BYTES like the C++ `Slice::substr` +
    /// `utf8_to_lower`; a multibyte code point split at the boundary must
    /// not panic.
    #[test]
    fn url_prefix_probes_are_byte_lenient() {
        assert_eq!(fix_url("123456789ü.de"), "123456789ü.de");
        assert_eq!(fix_url("12345678€.de"), "12345678€.de");
        assert_eq!(fix_url("hTtPs://üx.de/a"), "hTtPs://üx.de/a");
        assert_eq!(fix_url("123456789ü.xyzabc"), "");

        let text = FormattedText {
            text: "1234567€x.com/p".to_string(),
            entities: vec![MessageEntity {
                r#type: Some(Type::Url),
                offset: 0,
                length: 15,
                ..Default::default()
            }],
        };
        assert_eq!(get_first_url(&text), "1234567€x.com/p");

        let text = FormattedText {
            text: String::new(),
            entities: vec![MessageEntity {
                r#type: Some(Type::TextUrl),
                offset: 0,
                length: 1,
                argument: "http://€x.com".to_string(),
                ..Default::default()
            }],
        };
        assert_eq!(get_first_url(&text), "http://€x.com");
    }

    /// `get_first_url` skip rules (MessageEntity.cpp:1846-1851, 1876-1879):
    /// tg:-prefixed, exactly `ton:`/`ftp:`, `tonsite:`-prefixed TextUrl
    /// arguments and plain-domain Url entities are skipped; `ton://` and
    /// `ftp://` fall through because the C++ compares the lowercased first
    /// 8 bytes against the bare scheme names.
    #[test]
    fn get_first_url_skip_rules() {
        for argument in ["tg://emoji", "TG://x", "ton:", "ftp:", "tonsite:x"] {
            let text = FormattedText {
                text: String::new(),
                entities: vec![MessageEntity {
                    r#type: Some(Type::TextUrl),
                    offset: 0,
                    length: 1,
                    argument: argument.to_string(),
                    ..Default::default()
                }],
            };
            assert_eq!(get_first_url(&text), "", "{argument}");
        }
        for argument in ["ftp://x.com", "ton://x", "example.com"] {
            let text = FormattedText {
                text: String::new(),
                entities: vec![MessageEntity {
                    r#type: Some(Type::TextUrl),
                    offset: 0,
                    length: 1,
                    argument: argument.to_string(),
                    ..Default::default()
                }],
            };
            assert_eq!(get_first_url(&text), argument, "{argument}");
        }
        // A plain-domain Url entity is skipped; a negative offset wraps to
        // a huge size_t in the C++ and yields an empty URL, which
        // is_plain_domain rejects; a short Url entity is skipped before
        // the substring is taken.
        let plain = FormattedText {
            text: "example.com".to_string(),
            entities: vec![MessageEntity {
                r#type: Some(Type::Url),
                offset: 0,
                length: 11,
                ..Default::default()
            }],
        };
        assert_eq!(get_first_url(&plain), "");
        let negative = FormattedText {
            text: "https://example.com".to_string(),
            entities: vec![MessageEntity {
                r#type: Some(Type::Url),
                offset: -3,
                length: 19,
                ..Default::default()
            }],
        };
        assert_eq!(get_first_url(&negative), "");
        let short = FormattedText {
            text: "https://example.com".to_string(),
            entities: vec![MessageEntity {
                r#type: Some(Type::Url),
                offset: 0,
                length: 4,
                ..Default::default()
            }],
        };
        assert_eq!(get_first_url(&short), "");
    }

    /// `is_visible_url` (MessageEntity.cpp:1907-1927): only a Url entity
    /// with the same UTF-16 length whose slice equals the given URL counts.
    #[test]
    fn visible_url_matching() {
        let text = FormattedText {
            text: "go to example.com now".to_string(),
            entities: vec![MessageEntity {
                r#type: Some(Type::Url),
                offset: 6,
                length: 11,
                ..Default::default()
            }],
        };
        assert!(is_visible_url(&text, "example.com"));
        assert!(!is_visible_url(&text, "example.org"));
        assert!(!is_visible_url(&text, ""));
    }

    #[test]
    fn type_display_names() {
        // MessageEntity.cpp:66-119 — note FormattedDate prints as "Date".
        assert_eq!(Type::Mention.to_string(), "Mention");
        assert_eq!(Type::BotCommand.to_string(), "BotCommand");
        assert_eq!(Type::FormattedDate.to_string(), "Date");
        assert_eq!(
            Type::ExpandableBlockQuote.to_string(),
            "ExpandableBlockQuote"
        );
    }

    #[test]
    fn entity_display_dump() {
        // MessageEntity.cpp:121-140.
        assert_eq!(
            entity(Type::Bold, 3, 5).to_string(),
            "[Bold, offset = 3, length = 5]"
        );
        assert_eq!(
            MessageEntity::with_argument(Type::TextUrl, 0, 4, "https://x".to_owned()).to_string(),
            "[TextUrl, offset = 0, length = 4, argument = \"https://x\"]"
        );
        assert_eq!(
            MessageEntity::media_timestamp(1, 2, 42).to_string(),
            "[MediaTimestamp, offset = 1, length = 2, media_timestamp = \"42\"]"
        );
        assert_eq!(
            MessageEntity::mention_name(1, 2, UserId::new(77)).to_string(),
            "[MentionName, offset = 1, length = 2, user 77]"
        );
        assert_eq!(
            MessageEntity::custom_emoji(1, 2, CustomEmojiId::new(9)).to_string(),
            "[CustomEmoji, offset = 1, length = 2, custom emoji 9]"
        );
        assert_eq!(
            MessageEntity::formatted_date(
                1,
                2,
                FormattedDate::from_raw(1000, date_flags::RELATIVE)
            )
            .to_string(),
            "[Date, offset = 1, length = 2, at date 1000 with flags 1]"
        );
        // A negative media_timestamp is not printed (the C++ guard).
        let mut e = MessageEntity::media_timestamp(1, 2, -1);
        e.media_timestamp = -1;
        assert_eq!(e.to_string(), "[MediaTimestamp, offset = 1, length = 2]");
    }

    #[test]
    fn entity_ordering() {
        // operator<: offset asc, length desc, priority asc.
        let mut v = vec![
            entity(Type::Bold, 0, 4), // (0, 4, 90)
            entity(Type::Url, 0, 4),  // (0, 4, 50)
            entity(Type::Bold, 0, 6), // (0, 6, 90) — longer first
            entity(Type::Code, 1, 1), // (1, ...)
        ];
        v.sort();
        assert_eq!(
            v,
            vec![
                entity(Type::Bold, 0, 6),
                entity(Type::Url, 0, 4),
                entity(Type::Bold, 0, 4),
                entity(Type::Code, 1, 1),
            ]
        );
    }

    #[test]
    fn type_object_golden_json() {
        // get_text_entity_type_object — hand-written goldens from the C++
        // converter (MessageEntity.cpp:142-198).
        let dump = |e: &MessageEntity| json(&e.get_text_entity_type_object::<()>(None));
        assert_eq!(
            dump(&entity(Type::Mention, 0, 1)),
            r#"{"@type":"textEntityTypeMention"}"#
        );
        assert_eq!(
            dump(&entity(Type::Hashtag, 0, 1)),
            r#"{"@type":"textEntityTypeHashtag"}"#
        );
        assert_eq!(
            dump(&entity(Type::BotCommand, 0, 1)),
            r#"{"@type":"textEntityTypeBotCommand"}"#
        );
        assert_eq!(
            dump(&entity(Type::Url, 0, 1)),
            r#"{"@type":"textEntityTypeUrl"}"#
        );
        assert_eq!(
            dump(&entity(Type::EmailAddress, 0, 1)),
            r#"{"@type":"textEntityTypeEmailAddress"}"#
        );
        assert_eq!(
            dump(&entity(Type::Bold, 0, 1)),
            r#"{"@type":"textEntityTypeBold"}"#
        );
        assert_eq!(
            dump(&entity(Type::Italic, 0, 1)),
            r#"{"@type":"textEntityTypeItalic"}"#
        );
        assert_eq!(
            dump(&entity(Type::Underline, 0, 1)),
            r#"{"@type":"textEntityTypeUnderline"}"#
        );
        assert_eq!(
            dump(&entity(Type::Strikethrough, 0, 1)),
            r#"{"@type":"textEntityTypeStrikethrough"}"#
        );
        assert_eq!(
            dump(&entity(Type::BlockQuote, 0, 1)),
            r#"{"@type":"textEntityTypeBlockQuote"}"#
        );
        assert_eq!(
            dump(&entity(Type::Code, 0, 1)),
            r#"{"@type":"textEntityTypeCode"}"#
        );
        assert_eq!(
            dump(&entity(Type::Pre, 0, 1)),
            r#"{"@type":"textEntityTypePre"}"#
        );
        assert_eq!(
            dump(&MessageEntity::with_argument(
                Type::PreCode,
                0,
                1,
                "rust".to_owned()
            )),
            r#"{"@type":"textEntityTypePreCode","language":"rust"}"#
        );
        assert_eq!(
            dump(&MessageEntity::with_argument(
                Type::TextUrl,
                0,
                1,
                "https://t.me".to_owned()
            )),
            r#"{"@type":"textEntityTypeTextUrl","url":"https://t.me"}"#
        );
        assert_eq!(
            dump(&MessageEntity::mention_name(0, 1, UserId::new(42))),
            r#"{"@type":"textEntityTypeMentionName","user_id":42}"#
        );
        assert_eq!(
            dump(&entity(Type::Cashtag, 0, 1)),
            r#"{"@type":"textEntityTypeCashtag"}"#
        );
        assert_eq!(
            dump(&entity(Type::PhoneNumber, 0, 1)),
            r#"{"@type":"textEntityTypePhoneNumber"}"#
        );
        assert_eq!(
            dump(&entity(Type::BankCardNumber, 0, 1)),
            r#"{"@type":"textEntityTypeBankCardNumber"}"#
        );
        assert_eq!(
            dump(&MessageEntity::media_timestamp(0, 1, 123)),
            r#"{"@type":"textEntityTypeMediaTimestamp","media_timestamp":123}"#
        );
        assert_eq!(
            dump(&entity(Type::Spoiler, 0, 1)),
            r#"{"@type":"textEntityTypeSpoiler"}"#
        );
        assert_eq!(
            dump(&MessageEntity::custom_emoji(0, 1, CustomEmojiId::new(5))),
            r#"{"@type":"textEntityTypeCustomEmoji","custom_emoji_id":5}"#
        );
        assert_eq!(
            dump(&entity(Type::ExpandableBlockQuote, 0, 1)),
            r#"{"@type":"textEntityTypeExpandableBlockQuote"}"#
        );
        // The date entity: no formatting requested dumps the C++ null
        // as-is — since the M.10 union the generated td_api carries the
        // `Option` through, so the S09 default-Relative stand-in is gone.
        assert_eq!(
            dump(&MessageEntity::formatted_date(
                0,
                1,
                FormattedDate::from_raw(1000, 0)
            )),
            r#"{"@type":"textEntityTypeDateTime","unix_time":1000,"formatting_type":null}"#
        );
        assert_eq!(
            dump(&MessageEntity::formatted_date(
                0,
                1,
                FormattedDate::from_raw(1000, date_flags::SHORT_TIME)
            )),
            r#"{"@type":"textEntityTypeDateTime","unix_time":1000,"formatting_type":{"@type":"dateTimeFormattingTypeAbsolute","time_precision":{"@type":"dateTimePartPrecisionShort"},"date_precision":{"@type":"dateTimePartPrecisionNone"},"show_day_of_week":false}}"#
        );
    }

    #[test]
    fn mention_name_resolution_goes_through_the_manager() {
        struct LoggingResolver(Vec<i64>);
        impl UserManagerLike for LoggingResolver {
            fn resolve_user_id_object(&mut self, user_id: UserId, source: &str) -> i64 {
                assert_eq!(source, "textEntityTypeMentionName");
                self.0.push(user_id.get());
                user_id.get() + 1000 // observable pass-through
            }
            fn ensure_input_user(&mut self, _: UserId) -> Result<(), Status> {
                Ok(())
            }
            fn have_min_user(&self, _: UserId) -> bool {
                false
            }
            fn get_input_user_force(&mut self, _: UserId) -> tdmtproto::telegram_api::InputUser {
                unreachable!()
            }
        }
        let mut resolver = LoggingResolver(Vec::new());
        let obj = MessageEntity::mention_name(0, 1, UserId::new(7))
            .get_text_entity_type_object(Some(&mut resolver));
        assert_eq!(
            json(&obj),
            r#"{"@type":"textEntityTypeMentionName","user_id":1007}"#
        );
        assert_eq!(resolver.0, vec![7]);
    }

    #[test]
    fn text_entity_object_golden_json() {
        // get_text_entity_object (MessageEntity.cpp:200-202).
        let dump = |e: &MessageEntity| json(&e.get_text_entity_object::<()>(None));
        assert_eq!(
            dump(&MessageEntity::with_argument(
                Type::PreCode,
                2,
                7,
                "rust".to_owned()
            )),
            r#"{"offset":2,"length":7,"type":{"@type":"textEntityTypePreCode","language":"rust"}}"#
        );
        assert_eq!(
            dump(&MessageEntity::media_timestamp(3, 1, 42)),
            r#"{"offset":3,"length":1,"type":{"@type":"textEntityTypeMediaTimestamp","media_timestamp":42}}"#
        );
        assert_eq!(
            dump(&entity(Type::Mention, 0, 1)),
            r#"{"offset":0,"length":1,"type":{"@type":"textEntityTypeMention"}}"#
        );
    }

    #[test]
    fn text_entity_object_and_skip_flags() {
        // get_text_entities_object (MessageEntity.cpp:204-224).
        let entities = vec![
            entity(Type::Mention, 0, 1),
            entity(Type::BotCommand, 2, 3),
            MessageEntity::media_timestamp(4, 5, 10),
            MessageEntity::media_timestamp(6, 7, 99),
        ];

        let plain = get_text_entities_object::<()>(None, &entities, false, i32::MAX);
        assert_eq!(plain.len(), 4);
        assert_eq!(
            json(&plain[0]),
            r#"{"offset":0,"length":1,"type":{"@type":"textEntityTypeMention"}}"#
        );

        let no_bots = get_text_entities_object::<()>(None, &entities, true, i32::MAX);
        assert_eq!(no_bots.len(), 3);

        let maxed = get_text_entities_object::<()>(None, &entities, false, 50);
        assert_eq!(maxed.len(), 3, "the 99s media timestamp is dropped");

        let both = get_text_entities_object::<()>(None, &entities, true, 50);
        assert_eq!(both.len(), 2);
    }

    #[test]
    fn formatted_text_object_and_display() {
        let text = FormattedText {
            text: "ab".to_owned(),
            entities: vec![entity(Type::Bold, 0, 1)],
        };
        let obj = get_formatted_text_object::<()>(None, &text, false, i32::MAX);
        assert_eq!(
            json(&obj),
            r#"{"text":"ab","entities":[{"offset":0,"length":1,"type":{"@type":"textEntityTypeBold"}}]}"#
        );
        assert_eq!(
            text.to_string(),
            r#""ab" with entities [[Bold, offset = 0, length = 1]]"#
        );
        let empty = FormattedText::default();
        assert_eq!(empty.to_string(), "\"\" with entities []");
    }

    #[test]
    fn entity_store_parse_round_trip() {
        let entities = vec![
            entity(Type::Mention, 0, 1),
            MessageEntity::with_argument(Type::PreCode, 1, 2, "rs".to_owned()),
            MessageEntity::with_argument(Type::TextUrl, 3, 4, "https://t.me".to_owned()),
            MessageEntity::mention_name(5, 6, UserId::new(123456789)),
            MessageEntity::media_timestamp(7, 8, 77),
            MessageEntity::custom_emoji(9, 10, CustomEmojiId::new(11)),
            MessageEntity::formatted_date(
                12,
                13,
                FormattedDate::from_raw(1000, date_flags::LONG_TIME),
            ),
            // A Size-typed entity round-trips through the sentinel.
            MessageEntity {
                r#type: None,
                offset: 14,
                length: 15,
                ..MessageEntity::default()
            },
        ];
        let mut out = Vec::new();
        for e in &entities {
            e.store(&mut out);
        }
        let mut parser = crate::store_parse::Parser::new(&out);
        let mut back = Vec::new();
        for _ in 0..entities.len() {
            back.push(MessageEntity::parse(&mut parser).unwrap());
        }
        parser.fetch_end().unwrap();
        assert_eq!(back, entities);
        assert_eq!(back[7].r#type, None);
    }

    #[test]
    fn formatted_text_store_parse_round_trip_and_cleanup() {
        let text = FormattedText {
            text: "hi".to_owned(),
            entities: vec![
                entity(Type::Bold, 0, 2),
                entity(Type::Url, 0, 0), // empty — dropped by parse
            ],
        };
        let mut out = Vec::new();
        text.store(&mut out);
        let mut parser = crate::store_parse::Parser::new(&out);
        let back = FormattedText::parse(&mut parser).unwrap();
        parser.fetch_end().unwrap();
        // FormattedText::parse runs remove_empty_entities (hpp:68-73), so the
        // result differs from the stored input.
        assert_eq!(back.entities.len(), 1);
        assert_eq!(back.entities[0], entity(Type::Bold, 0, 2));
        assert_eq!(back.text, "hi");
    }

    #[test]
    fn remove_empty_entities_rules() {
        // MessageEntity.cpp:1444-1466.
        let mut entities = vec![
            entity(Type::Bold, 0, 2),                                           // kept
            entity(Type::Bold, 0, 0),                                           // length == 0
            entity(Type::Italic, 0, -1),                                        // negative length
            MessageEntity::with_argument(Type::TextUrl, 0, 2, String::new()),   // empty url
            MessageEntity::with_argument(Type::TextUrl, 0, 2, "x".to_owned()),  // kept
            MessageEntity::mention_name(0, 2, UserId::new(0)),                  // invalid user
            MessageEntity::mention_name(0, 2, UserId::new(1)),                  // kept
            MessageEntity::custom_emoji(0, 2, CustomEmojiId::new(0)),           // invalid id
            MessageEntity::custom_emoji(0, 2, CustomEmojiId::new(1)),           // kept
            MessageEntity::formatted_date(0, 2, FormattedDate::from_raw(0, 1)), // invalid date
            MessageEntity::media_timestamp(0, 2, 0), // kept — no MediaTimestamp rule
        ];
        remove_empty_entities(&mut entities);
        assert_eq!(
            entities,
            vec![
                entity(Type::Bold, 0, 2),
                MessageEntity::with_argument(Type::TextUrl, 0, 2, "x".to_owned()),
                MessageEntity::mention_name(0, 2, UserId::new(1)),
                MessageEntity::custom_emoji(0, 2, CustomEmojiId::new(1)),
                MessageEntity::media_timestamp(0, 2, 0),
            ]
        );
    }

    #[test]
    fn type_mask_predicates() {
        // MessageEntity.cpp:1488-1560.
        use Type::*;
        assert!(is_splittable_entity(Bold) && is_splittable_entity(Italic));
        assert!(is_splittable_entity(Underline) && is_splittable_entity(Strikethrough));
        assert!(is_splittable_entity(Spoiler));
        assert!(!is_splittable_entity(Code));
        assert!(is_blockquote_entity(BlockQuote) && is_blockquote_entity(ExpandableBlockQuote));
        assert!(!is_blockquote_entity(Bold));
        for t in [
            Mention,
            Hashtag,
            BotCommand,
            Url,
            EmailAddress,
            TextUrl,
            MentionName,
            Cashtag,
            PhoneNumber,
            BankCardNumber,
            MediaTimestamp,
            CustomEmoji,
        ] {
            assert!(is_continuous_entity(t));
        }
        assert!(!is_continuous_entity(Bold));
        for t in [Pre, Code, PreCode, FormattedDate] {
            assert!(is_pre_entity(t));
        }
        assert!(!is_pre_entity(BlockQuote));
        // user = splittable ∪ blockquote ∪ {TextUrl, MentionName, CustomEmoji} ∪ pre
        for t in [
            Bold,
            Italic,
            Underline,
            Strikethrough,
            Spoiler,
            BlockQuote,
            ExpandableBlockQuote,
            TextUrl,
            MentionName,
            CustomEmoji,
            Pre,
            Code,
            PreCode,
            FormattedDate,
        ] {
            assert!(is_user_entity(t), "{t:?} must be a user entity");
        }
        for t in [
            Mention,
            Hashtag,
            BotCommand,
            Url,
            EmailAddress,
            Cashtag,
            PhoneNumber,
            BankCardNumber,
            MediaTimestamp,
        ] {
            assert!(!is_user_entity(t), "{t:?} must not be a user entity");
        }
        // splittable index order: Bold, Italic, Underline, Strikethrough, Spoiler.
        assert_eq!(splittable_entity_type_index(Bold), 0);
        assert_eq!(splittable_entity_type_index(Italic), 1);
        assert_eq!(splittable_entity_type_index(Underline), 2);
        assert_eq!(splittable_entity_type_index(Strikethrough), 3);
        assert_eq!(splittable_entity_type_index(Spoiler), 4);
    }

    #[test]
    fn td_api_entities_round_trip() {
        // td_api in -> MessageEntity -> td_api out, allow_all.
        let input = vec![
            td_api::TextEntity {
                offset: 0,
                length: 2,
                r#type: Some(td_api::TextEntityType::textEntityTypeBold(
                    td_api::textEntityTypeBold {},
                )),
            },
            td_api::TextEntity {
                offset: 3,
                length: 4,
                r#type: Some(td_api::TextEntityType::textEntityTypePreCode(
                    td_api::textEntityTypePreCode {
                        language: "rust".to_owned(),
                    },
                )),
            },
            td_api::TextEntity {
                offset: 8,
                length: 9,
                r#type: Some(td_api::TextEntityType::textEntityTypeUrl(
                    td_api::textEntityTypeUrl {},
                )),
            },
        ];
        let entities = get_message_entities::<()>(None, input.clone(), true).unwrap();
        assert_eq!(entities.len(), 3);
        assert_eq!(entities[0].r#type, Some(Type::Bold));
        assert_eq!(entities[1].r#type, Some(Type::PreCode));
        assert_eq!(entities[1].argument, "rust");
        assert_eq!(entities[2].r#type, Some(Type::Url));

        let back = get_text_entities_object::<()>(None, &entities, false, i32::MAX);
        assert_eq!(json(&back), json(&input));
    }

    #[test]
    fn td_api_entities_drop_non_user_entities_unless_allowed() {
        let input = |r#type| {
            vec![td_api::TextEntity {
                offset: 0,
                length: 2,
                r#type: Some(r#type),
            }]
        };
        // Url is not a user entity: dropped without allow_all.
        assert!(
            get_message_entities::<()>(
                None,
                input(td_api::TextEntityType::textEntityTypeUrl(
                    td_api::textEntityTypeUrl {}
                )),
                false
            )
            .unwrap()
            .is_empty()
        );
        // Bold survives.
        assert_eq!(
            get_message_entities::<()>(
                None,
                input(td_api::TextEntityType::textEntityTypeBold(
                    td_api::textEntityTypeBold {}
                )),
                false
            )
            .unwrap()
            .len(),
            1
        );
    }

    #[test]
    fn td_api_text_url_becomes_mention_name_for_tg_user_links() {
        // MessageEntity.cpp:3743-3757.
        let input = vec![td_api::TextEntity {
            offset: 0,
            length: 5,
            r#type: Some(td_api::TextEntityType::textEntityTypeTextUrl(
                td_api::textEntityTypeTextUrl {
                    url: "https://t.me".to_owned(),
                },
            )),
        }];
        // The plain URL keeps the TextUrl type with the checked link
        // (check_link appends the trailing '/').
        let entities = get_message_entities::<()>(None, input.clone(), true).unwrap();
        assert_eq!(entities[0].r#type, Some(Type::TextUrl));
        assert_eq!(entities[0].argument, "https://t.me/");

        // An invalid URL is a 400 with the "Entity " prefix.
        let bad = vec![td_api::TextEntity {
            offset: 0,
            length: 5,
            r#type: Some(td_api::TextEntityType::textEntityTypeTextUrl(
                td_api::textEntityTypeTextUrl {
                    url: "http://localhost/".to_owned(),
                },
            )),
        }];
        let error = get_message_entities::<()>(None, bad, true).unwrap_err();
        assert_eq!(error.code(), 400);
        // "Entity " + check_link's wrapped message (LinkManager.cpp:1929-1936).
        assert_eq!(
            error.message(),
            "Entity URL 'http://localhost/' is invalid: Wrong HTTP URL"
        );

        // tg:user links resolve through the manager validation.
        struct ValidatingManager(bool);
        impl UserManagerLike for ValidatingManager {
            fn resolve_user_id_object(&mut self, user_id: UserId, _source: &str) -> i64 {
                user_id.get()
            }
            fn ensure_input_user(&mut self, user_id: UserId) -> Result<(), Status> {
                assert_eq!(user_id.get(), 123456789);
                if self.0 {
                    Ok(())
                } else {
                    Err(Status::error(400, "Have no access to the user"))
                }
            }
            fn have_min_user(&self, _user_id: UserId) -> bool {
                true
            }
            fn get_input_user_force(
                &mut self,
                _user_id: UserId,
            ) -> tdmtproto::telegram_api::InputUser {
                unreachable!()
            }
        }
        let tg_user = vec![td_api::TextEntity {
            offset: 0,
            length: 5,
            r#type: Some(td_api::TextEntityType::textEntityTypeTextUrl(
                td_api::textEntityTypeTextUrl {
                    url: "tg://user?id=123456789".to_owned(),
                },
            )),
        }];
        let ok = get_message_entities(Some(&mut ValidatingManager(true)), tg_user.clone(), true)
            .unwrap();
        assert_eq!(ok[0].r#type, Some(Type::MentionName));
        assert_eq!(ok[0].user_id.get(), 123456789);
        let err = get_message_entities(Some(&mut ValidatingManager(false)), tg_user.clone(), true)
            .unwrap_err();
        assert_eq!(err.message(), "Have no access to the user");
        // With no manager the validation is skipped (the nullptr path).
        let ok = get_message_entities::<()>(None, tg_user, true).unwrap();
        assert_eq!(ok[0].r#type, Some(Type::MentionName));
    }

    #[test]
    fn td_api_entity_validation_errors() {
        let media = vec![td_api::TextEntity {
            offset: 0,
            length: 2,
            r#type: Some(td_api::TextEntityType::textEntityTypeMediaTimestamp(
                td_api::textEntityTypeMediaTimestamp {
                    media_timestamp: -1,
                },
            )),
        }];
        let error = get_message_entities::<()>(None, media, true).unwrap_err();
        assert_eq!(
            (error.code(), error.message()),
            (400, "Invalid media timestamp specified")
        );

        let emoji = vec![td_api::TextEntity {
            offset: 0,
            length: 2,
            r#type: Some(td_api::TextEntityType::textEntityTypeCustomEmoji(
                td_api::textEntityTypeCustomEmoji { custom_emoji_id: 0 },
            )),
        }];
        let error = get_message_entities::<()>(None, emoji, true).unwrap_err();
        assert_eq!(error.message(), "Invalid custom emoji identifier specified");

        // The date entity validates through FormattedDate.
        let date = vec![td_api::TextEntity {
            offset: 0,
            length: 2,
            r#type: Some(td_api::TextEntityType::textEntityTypeDateTime(
                td_api::textEntityTypeDateTime {
                    unix_time: 0,
                    formatting_type: Some(td_api::DateTimeFormattingType::default()),
                },
            )),
        }];
        let error = get_message_entities::<()>(None, date, true).unwrap_err();
        assert_eq!(error.message(), "Invalid date specified");
    }

    #[test]
    fn server_entities_convert() {
        use tdmtproto::telegram_api as tg;
        // Bold + Pre (empty language -> Pre; non-empty -> PreCode) +
        // blockquote collapsed flag + invalid MentionName skipped.
        let server = vec![
            tg::MessageEntity::messageEntityBold(tg::messageEntityBold {
                offset: 0,
                length: 1,
            }),
            tg::MessageEntity::messageEntityPre(tg::messageEntityPre {
                offset: 2,
                length: 3,
                language: String::new(),
            }),
            tg::MessageEntity::messageEntityPre(tg::messageEntityPre {
                offset: 4,
                length: 5,
                language: "rs".to_owned(),
            }),
            tg::MessageEntity::messageEntityBlockquote(tg::messageEntityBlockquote {
                flags: 0,
                collapsed: true,
                offset: 6,
                length: 7,
            }),
            tg::MessageEntity::messageEntityMentionName(tg::messageEntityMentionName {
                offset: 8,
                length: 9,
                user_id: 0, // invalid -> logged and skipped
            }),
            tg::MessageEntity::messageEntityUnknown(tg::messageEntityUnknown {
                offset: 0,
                length: 0,
            }),
        ];
        let entities = get_server_message_entities::<()>(None, server, "test");
        assert_eq!(
            entities,
            vec![
                entity(Type::Bold, 0, 1),
                entity(Type::Pre, 2, 3),
                MessageEntity::with_argument(Type::PreCode, 4, 5, "rs".to_owned()),
                entity(Type::ExpandableBlockQuote, 6, 7),
            ]
        );

        // A known MentionName survives only with min-user knowledge.
        struct MinUserManager;
        impl UserManagerLike for MinUserManager {
            fn resolve_user_id_object(&mut self, user_id: UserId, _s: &str) -> i64 {
                user_id.get()
            }
            fn ensure_input_user(&mut self, _: UserId) -> Result<(), Status> {
                Ok(())
            }
            fn have_min_user(&self, user_id: UserId) -> bool {
                user_id.get() == 5
            }
            fn get_input_user_force(&mut self, _: UserId) -> tdmtproto::telegram_api::InputUser {
                unreachable!()
            }
        }
        let server = vec![tg::MessageEntity::messageEntityMentionName(
            tg::messageEntityMentionName {
                offset: 0,
                length: 1,
                user_id: 5,
            },
        )];
        let entities = get_server_message_entities(Some(&mut MinUserManager), server, "test");
        assert_eq!(
            entities,
            vec![MessageEntity::mention_name(0, 1, UserId::new(5))]
        );
    }

    #[test]
    fn secret_entities_convert() {
        use tdmtproto::secret_api as sa;
        let secret = vec![
            sa::MessageEntity::messageEntityBold(sa::messageEntityBold {
                offset: 0,
                length: 1,
            }),
            // rediscovered by the finders — skipped
            sa::MessageEntity::messageEntityMention(sa::messageEntityMention {
                offset: 2,
                length: 3,
            }),
            sa::MessageEntity::messageEntityPre(sa::messageEntityPre {
                offset: 4,
                length: 5,
                language: b"rs".to_vec(),
            }),
            sa::MessageEntity::messageEntityCustomEmoji(sa::messageEntityCustomEmoji {
                offset: 6,
                length: 7,
                document_id: 8,
            }),
        ];
        let entities = get_secret_message_entities(secret, false, None);
        assert_eq!(
            entities,
            vec![
                entity(Type::Bold, 0, 1),
                MessageEntity::with_argument(Type::PreCode, 4, 5, "rs".to_owned()),
                MessageEntity::custom_emoji(6, 7, CustomEmojiId::new(8)),
            ]
        );

        // A premium sender keeps all custom emoji (cpp:4059 short-circuits);
        // with no checker (cold cache, remove_unknown=false) non-premium
        // senders keep them too; the checker filters premium-only emoji for
        // non-premium senders.
        let secret = vec![sa::MessageEntity::messageEntityCustomEmoji(
            sa::messageEntityCustomEmoji {
                offset: 0,
                length: 1,
                document_id: 42,
            },
        )];
        let entities = get_secret_message_entities(secret.clone(), true, None);
        assert_eq!(entities.len(), 1);
        let mut checker = |id: CustomEmojiId, _remove_unknown: bool| id.get() == 42;
        let entities = get_secret_message_entities(secret.clone(), false, Some(&mut checker));
        assert!(entities.is_empty());
        let mut checker = |_: CustomEmojiId, _remove_unknown: bool| false;
        let entities = get_secret_message_entities(secret, false, Some(&mut checker));
        assert_eq!(entities.len(), 1);
    }

    #[test]
    fn quote_entity_rules() {
        // is_allowed_quote_entity_type (MessageEntity.cpp:4840-4853).
        assert!(is_allowed_quote_entity_type(Type::Bold));
        assert!(is_allowed_quote_entity_type(Type::FormattedDate));
        assert!(!is_allowed_quote_entity_type(Type::Url));

        let mut text = FormattedText {
            text: "ab".to_owned(),
            entities: vec![entity(Type::Bold, 0, 1), entity(Type::Url, 1, 1)],
        };
        assert!(remove_unallowed_quote_entities(&mut text));
        assert_eq!(text.entities.len(), 1);
        assert!(!remove_unallowed_quote_entities(&mut text));

        let mut text = FormattedText {
            text: String::new(),
            entities: vec![entity(Type::Bold, 0, 1), entity(Type::CustomEmoji, 1, 1)],
        };
        assert!(keep_only_custom_emoji(&mut text));
        assert_eq!(text.entities.len(), 1);
    }

    #[test]
    fn parse_markdown_v1_bare_bracket_url() {
        // The local C++ v1 parser turns `[url]` into a TextUrl with the
        // bracket content as the URL ("use text as a URL",
        // MessageEntity.cpp:2005-2021); the TD_BUILD oracle corrupts this
        // branch, so the conformance golden pins the source behavior.
        let mut text = "[https://example.com]".to_owned();
        let entities = parse_markdown(&mut text).unwrap();
        assert_eq!(text, "https://example.com");
        assert_eq!(
            entities,
            vec![MessageEntity::with_argument(
                Type::TextUrl,
                0,
                19,
                "https://example.com/".to_owned()
            )]
        );

        // `[text](url)` keeps the explicit-url form.
        let mut text = "[text](https://t.me)".to_owned();
        let entities = parse_markdown(&mut text).unwrap();
        assert_eq!(text, "text");
        assert_eq!(
            entities,
            vec![MessageEntity::with_argument(
                Type::TextUrl,
                0,
                4,
                "https://t.me/".to_owned()
            )]
        );
    }

    /// M.8.6 golden vectors: the parse_markdown battery whose td_execute
    /// outputs are recorded as the last 41 entries of
    /// `testdata/message_entity_cpp_golden.jsonl` (all offsets/lengths are
    /// UTF-16 code units). The six bare-bracket entries hold the
    /// source-true answers — `fd53a8160`'s in-place compaction reads
    /// overwritten bytes there in the pinned C++ (and thus in the oracle),
    /// a read-after-overwrite bug the port deliberately does not copy (see
    /// the conformance test comment).
    #[test]
    fn parse_markdown_v1_golden_vectors() {
        fn parse(text: &str) -> (String, Vec<MessageEntity>) {
            let mut owned = text.to_owned();
            let entities = parse_markdown(&mut owned).unwrap();
            (owned, entities)
        }
        let bold = |offset, length| MessageEntity::new(Type::Bold, offset, length);
        let code = |offset, length| MessageEntity::new(Type::Code, offset, length);
        let pre = |offset, length| MessageEntity::new(Type::Pre, offset, length);
        let pre_code = |offset, length, language: &str| {
            MessageEntity::with_argument(Type::PreCode, offset, length, language.to_owned())
        };
        let text_url = |offset, length, url: &str| {
            MessageEntity::with_argument(Type::TextUrl, offset, length, url.to_owned())
        };

        // A `**`/`__` opener closes immediately (empty content) — the
        // trailing pairs delimit empty entities, so nothing is emitted.
        assert_eq!(
            parse("**bold** __underline__"),
            ("bold underline".to_owned(), vec![])
        );
        // Empty entity pairs are dropped entirely.
        for text in ["__", "**", "``", "[]"] {
            assert_eq!(parse(text), (String::new(), vec![]));
        }
        // Empty `` pairs inside plain text.
        assert_eq!(parse("a `` b"), ("a  b".to_owned(), vec![]));
        assert_eq!(
            parse("`code` and `` ``"),
            ("code and  ".to_owned(), vec![code(0, 4)])
        );

        // Pre blocks: language, one-newline skip (LF and CRLF), content.
        assert_eq!(
            parse("```python\nprint(1)\n```"),
            ("print(1)\n".to_owned(), vec![pre_code(0, 9, "python")])
        );
        assert_eq!(
            parse("```\r\nwin line\r\n```"),
            ("win line\r\n".to_owned(), vec![pre(0, 10)])
        );
        // The language ends at the space; the space starts the content.
        assert_eq!(
            parse("```lang code```"),
            (" code".to_owned(), vec![pre_code(0, 5, "lang")])
        );
        // Empty pre content emits no entity.
        assert_eq!(parse("```c\n```"), (String::new(), vec![]));
        assert_eq!(
            parse("x ``` y ``` z"),
            ("x  y  z".to_owned(), vec![pre(2, 3)])
        );
        // The second opener's newline is skipped and the next ``` closes an
        // empty pre (the C++ fd53a8160 behavior the oracle agrees on).
        assert_eq!(parse("```a\n```b```\n```"), ("b".to_owned(), vec![]));

        // UTF-16 offsets: 4-byte emoji count as 2, combining chars as 1.
        assert_eq!(
            parse("\u{1f600}*bold* \u{1f400}`code`"),
            (
                "\u{1f600}bold \u{1f400}code".to_owned(),
                vec![bold(2, 4), code(9, 4)]
            )
        );
        assert_eq!(parse("а́*б*"), ("а́б".to_owned(), vec![bold(2, 1)]));

        // Escapes drop the backslash before _ * ` [; other backslashes stay.
        assert_eq!(parse("\\_ \\* \\` \\["), ("_ * ` [".to_owned(), vec![]));
        assert_eq!(parse("\\\\_"), ("\\_".to_owned(), vec![]));
        assert_eq!(parse("\\x"), ("\\x".to_owned(), vec![]));
        assert_eq!(parse("abc\\"), ("abc\\".to_owned(), vec![]));

        // Nested markers are not parsed in v1 — the inner `_` is content.
        assert_eq!(
            parse("*bold _italic_ bold*"),
            ("bold _italic_ bold".to_owned(), vec![bold(0, 18)])
        );

        // Link entities: the explicit-url form reads the pristine `(url)`.
        assert_eq!(
            parse("[link](https://example.com/path?q=1&x=2)"),
            (
                "link".to_owned(),
                vec![text_url(0, 4, "https://example.com/path?q=1&x=2")]
            )
        );
        // mailto: the userinfo host split keeps the whole userinfo.
        assert_eq!(
            parse("[x](mailto:a@b.com)"),
            (
                "x".to_owned(),
                vec![text_url(0, 1, "http://mailto:a@b.com/")]
            )
        );
        assert_eq!(
            parse("[a](tg://resolve?domain=x)"),
            (
                "a".to_owned(),
                vec![text_url(0, 1, "tg://resolve?domain=x")]
            )
        );
        // Invalid URLs emit no entity.
        assert_eq!(parse("[x]()"), ("x".to_owned(), vec![]));
        assert_eq!(parse("**[t](u)**"), ("t".to_owned(), vec![]));

        // Unclosed entities are 400 errors with the byte offset.
        for (text, offset) in [
            ("*abc", 0),
            ("`abc", 0),
            ("[abc", 0),
            ("x [abc", 2),
            ("x `abc", 2),
            ("x _abc", 2),
        ] {
            let mut owned = text.to_owned();
            let error = parse_markdown(&mut owned).unwrap_err();
            assert_eq!(
                (error.code(), error.message()),
                (
                    400,
                    format!("Can't find end of the entity starting at byte offset {offset}")
                        .as_str()
                )
            );
        }

        // Bare brackets use the bracket content as the URL (source-true).
        assert_eq!(
            parse("[t.me]"),
            ("t.me".to_owned(), vec![text_url(0, 4, "http://t.me/")])
        );
        assert_eq!(
            parse("[https://example.com]"),
            (
                "https://example.com".to_owned(),
                vec![text_url(0, 19, "https://example.com/")]
            )
        );
        assert_eq!(
            parse("[example.com/path]"),
            (
                "example.com/path".to_owned(),
                vec![text_url(0, 16, "http://example.com/path")]
            )
        );
        // tg://user links become MentionNames.
        assert_eq!(
            parse("[tg://user?id=42]"),
            (
                "tg://user?id=42".to_owned(),
                vec![MessageEntity::mention_name(0, 15, UserId::new(42))]
            )
        );
        assert_eq!(
            parse("[tg://emoji?id=5]"),
            (
                "tg://emoji?id=5".to_owned(),
                vec![text_url(0, 15, "tg://emoji?id=5")]
            )
        );
        assert_eq!(
            parse("[ton://resolve?domain=x]"),
            (
                "ton://resolve?domain=x".to_owned(),
                vec![text_url(0, 22, "ton://resolve?domain=x")]
            )
        );
    }

    // ===== M.8.7 — Markdown v2 =====

    /// M.8.7 hand-traced vectors for `parse_markdown_v2`
    /// (MessageEntity.cpp:2048-2399): nested entities, escapes, pre/code
    /// narrowing, expandable blockquotes, custom emoji/FormattedDate links
    /// and the 400 error paths — all pinning the source semantics. The
    /// `td_execute` oracle corpus for the same dialect is the last 76
    /// entries of `testdata/message_entity_cpp_golden.jsonl`.
    #[test]
    fn parse_markdown_v2_golden_vectors() {
        fn parse(text: &str) -> (String, Vec<MessageEntity>) {
            let mut owned = text.to_owned();
            let entities = parse_markdown_v2(&mut owned).unwrap();
            (owned, entities)
        }
        let bold = |offset, length| MessageEntity::new(Type::Bold, offset, length);
        let italic = |offset, length| MessageEntity::new(Type::Italic, offset, length);
        let underline = |offset, length| MessageEntity::new(Type::Underline, offset, length);
        let strike = |offset, length| MessageEntity::new(Type::Strikethrough, offset, length);
        let spoiler = |offset, length| MessageEntity::new(Type::Spoiler, offset, length);
        let code = |offset, length| MessageEntity::new(Type::Code, offset, length);
        let pre = |offset, length| MessageEntity::new(Type::Pre, offset, length);
        let pre_code = |offset, length, language: &str| {
            MessageEntity::with_argument(Type::PreCode, offset, length, language.to_owned())
        };
        let blockquote = |offset, length| MessageEntity::new(Type::BlockQuote, offset, length);
        let expandable =
            |offset, length| MessageEntity::new(Type::ExpandableBlockQuote, offset, length);
        let text_url = |offset, length, url: &str| {
            MessageEntity::with_argument(Type::TextUrl, offset, length, url.to_owned())
        };

        // v2 delimiters are single characters; `**`/`__`/`~~` open and
        // immediately close an empty entity, which is then dropped.
        assert_eq!(parse("*bold*"), ("bold".to_owned(), vec![bold(0, 4)]));
        assert_eq!(parse("_italic_"), ("italic".to_owned(), vec![italic(0, 6)]));
        assert_eq!(
            parse("__underline__"),
            ("underline".to_owned(), vec![underline(0, 9)])
        );
        assert_eq!(parse("~strike~"), ("strike".to_owned(), vec![strike(0, 6)]));
        assert_eq!(parse("~~strike~~"), ("strike".to_owned(), vec![]));
        assert_eq!(
            parse("||spoiler||"),
            ("spoiler".to_owned(), vec![spoiler(0, 7)])
        );
        assert_eq!(parse("`code`"), ("code".to_owned(), vec![code(0, 4)]));

        // Escapes drop the backslash before any ASCII char in 1..=126.
        assert_eq!(parse("a\\.b"), ("a.b".to_owned(), vec![]));
        assert_eq!(parse("\\\\"), ("\\".to_owned(), vec![]));
        assert_eq!(parse("abc\\"), ("abc\\".to_owned(), vec![]));

        // Nested entities: the inner entity is emitted at its own offsets.
        assert_eq!(
            parse("*bold _italic_ bold*"),
            (
                "bold italic bold".to_owned(),
                vec![bold(0, 16), italic(5, 6)]
            )
        );
        assert_eq!(
            parse("_italic *bold* italic_"),
            (
                "italic bold italic".to_owned(),
                vec![italic(0, 18), bold(7, 4)]
            )
        );
        assert_eq!(
            parse("*bold `code` bold*"),
            ("bold code bold".to_owned(), vec![bold(0, 14), code(5, 4)])
        );
        // Inside code the reserved set narrows to the backtick.
        assert_eq!(
            parse("`code *star*`"),
            ("code *star*".to_owned(), vec![code(0, 11)])
        );
        assert_eq!(
            parse("*a _b_ c*"),
            ("a b c".to_owned(), vec![bold(0, 5), italic(2, 1)])
        );

        // Pre blocks: language, one-newline skip (LF and CRLF), content.
        assert_eq!(
            parse("```python\ncode\n```"),
            ("code\n".to_owned(), vec![pre_code(0, 5, "python")])
        );
        assert_eq!(
            parse("```\r\nwin\r\n```"),
            ("win\r\n".to_owned(), vec![pre(0, 5)])
        );
        // The language ends at the space; the space starts the content.
        assert_eq!(
            parse("```lang code```"),
            (" code".to_owned(), vec![pre_code(0, 5, "lang")])
        );
        assert_eq!(parse("```c\n```"), (String::new(), vec![]));

        // Blockquotes: `>` at line starts, content until the last line.
        assert_eq!(
            parse(">quote"),
            ("quote".to_owned(), vec![blockquote(0, 5)])
        );
        assert_eq!(
            parse(">line1\n>line2"),
            ("line1\nline2".to_owned(), vec![blockquote(0, 11)])
        );
        assert_eq!(
            parse(">line1\nafter"),
            ("line1\nafter".to_owned(), vec![blockquote(0, 6)])
        );
        // The unclosed `||` spoiler makes the blockquote expandable: it is
        // dropped from the stack and the newline becomes the last line
        // (the entity_byte_offset is the first `|`).
        assert_eq!(parse(">||\n"), ("\n".to_owned(), vec![expandable(0, 1)]));
        assert_eq!(
            parse(">||\r\n"),
            ("\r\n".to_owned(), vec![expandable(0, 2)])
        );
        // Empty expandable blockquotes emit nothing.
        assert_eq!(parse(">||"), (String::new(), vec![]));
        // A closed spoiler inside a blockquote stays a blockquote.
        assert_eq!(
            parse(">||x||"),
            ("x".to_owned(), vec![blockquote(0, 1), spoiler(0, 1)])
        );

        // TextUrl: the explicit form, the bare-bracket form (the bracket
        // content is the URL, escapes dropped) and invalid URLs.
        assert_eq!(
            parse("[text](https://t.me/durov)"),
            (
                "text".to_owned(),
                vec![text_url(0, 4, "https://t.me/durov")]
            )
        );
        assert_eq!(
            parse("[t\\.me/durov]"),
            (
                "t.me/durov".to_owned(),
                vec![text_url(0, 10, "http://t.me/durov")]
            )
        );
        assert_eq!(parse("[text only]"), ("text only".to_owned(), vec![]));
        // tg://user links become MentionNames.
        assert_eq!(
            parse("[n](tg://user?id=42)"),
            (
                "n".to_owned(),
                vec![MessageEntity::mention_name(0, 1, UserId::new(42))]
            )
        );
        assert_eq!(
            parse("[tg://user?id\\=42]"),
            (
                "tg://user?id=42".to_owned(),
                vec![MessageEntity::mention_name(0, 15, UserId::new(42))]
            )
        );

        // Custom emoji and FormattedDate links.
        assert_eq!(
            parse("![\u{1f600}](tg://emoji?id=5)"),
            (
                "\u{1f600}".to_owned(),
                vec![MessageEntity::custom_emoji(0, 2, CustomEmojiId::new(5),)]
            )
        );
        let date = FormattedDate::get_formatted_date_by_format(1600000000, "r").unwrap();
        assert_eq!(
            parse("![x](tg://time?unix=1600000000&format=r)"),
            (
                "x".to_owned(),
                vec![MessageEntity::formatted_date(0, 1, date)]
            )
        );

        // UTF-16 offsets: 4-byte emoji count as 2, CJK as 1 each.
        assert_eq!(
            parse("\u{1f600}*bold*\u{1f400}"),
            ("\u{1f600}bold\u{1f400}".to_owned(), vec![bold(2, 4)])
        );
        assert_eq!(
            parse("中文*粗体*"),
            ("中文粗体".to_owned(), vec![bold(2, 2)])
        );
        // A bold entity spans a newline.
        assert_eq!(parse("*a\nb*"), ("a\nb".to_owned(), vec![bold(0, 3)]));

        // Unclosed entities are 400 errors with the C++ byte offset and the
        // C++ message shape "Can't find end of <type> entity at byte offset".
        for (text, message) in [
            (
                "```unterminated",
                "Can't find end of Pre entity at byte offset 0",
            ),
            (
                "`unterminated",
                "Can't find end of Code entity at byte offset 0",
            ),
            (
                "unclosed __u",
                "Can't find end of Underline entity at byte offset 9",
            ),
            (
                "||unclosed",
                "Can't find end of Spoiler entity at byte offset 0",
            ),
            (
                "[unclosed",
                "Can't find end of TextUrl entity at byte offset 0",
            ),
            (
                "[text](unclosed",
                "Can't find end of a URL at byte offset 7",
            ),
            (
                "*a _b* c_",
                "Can't find end of Italic entity at byte offset 8",
            ),
            (">||x", "Can't find end of Spoiler entity at byte offset 1"),
        ] {
            let mut owned = text.to_owned();
            let error = parse_markdown_v2(&mut owned).unwrap_err();
            assert_eq!((error.code(), error.message()), (400, message));
        }

        // Unescaped reserved characters are 400 errors naming the character.
        for (text, message) in [
            (
                "pre >x",
                "Character '>' is reserved and must be escaped with the preceding '\\'",
            ),
            (
                "a.b",
                "Character '.' is reserved and must be escaped with the preceding '\\'",
            ),
            (
                "a|b",
                "Character '|' is reserved and must be escaped with the preceding '\\'",
            ),
            (
                "a!b",
                "Character '!' is reserved and must be escaped with the preceding '\\'",
            ),
            (
                "a=b",
                "Character '=' is reserved and must be escaped with the preceding '\\'",
            ),
        ] {
            let mut owned = text.to_owned();
            let error = parse_markdown_v2(&mut owned).unwrap_err();
            assert_eq!((error.code(), error.message()), (400, message));
        }

        // Custom emoji link errors.
        let mut owned = "![x]".to_owned();
        let error = parse_markdown_v2(&mut owned).unwrap_err();
        let message: &str = "The entity must contain a tg://emoji or tg://time URL";
        assert_eq!((error.code(), error.message()), (400, message));
        let mut owned = "![x](tg://emoji)".to_owned();
        let error = parse_markdown_v2(&mut owned).unwrap_err();
        let message: &str = "Invalid tg://emoji or tg://time URL specified";
        assert_eq!((error.code(), error.message()), (400, message));
    }

    // ===== M.8.8 — Markdown v3 (MessageEntity.cpp:2401-3238) =====

    /// M.8.8 hand-traced vectors for `parse_markdown_v3`
    /// (MessageEntity.cpp:2992-3068) and `get_markdown_v3`
    /// (MessageEntity.cpp:3070-3238) on the direct functions: v3
    /// delimiters are doubled (`__`/`**`/`~~`/`||`), fences split pre
    /// entities, predetermined entities win over the syntax they cover and
    /// the formatter's parse-check returns the original text when the
    /// formatted text would not round-trip. The `td_execute` oracle corpus
    /// for the same dialect is the last 97 entries of
    /// `testdata/message_entity_cpp_golden.jsonl`.
    #[test]
    fn parse_markdown_v3_golden_vectors() {
        fn parse(text: &str) -> (String, Vec<MessageEntity>) {
            let result = parse_markdown_v3(FormattedText {
                text: text.to_owned(),
                entities: Vec::new(),
            });
            (result.text, result.entities)
        }
        fn parse_with(text: &str, entities: Vec<MessageEntity>) -> (String, Vec<MessageEntity>) {
            let result = parse_markdown_v3(FormattedText {
                text: text.to_owned(),
                entities,
            });
            (result.text, result.entities)
        }
        let bold = |offset, length| MessageEntity::new(Type::Bold, offset, length);
        let italic = |offset, length| MessageEntity::new(Type::Italic, offset, length);
        let strike = |offset, length| MessageEntity::new(Type::Strikethrough, offset, length);
        let spoiler = |offset, length| MessageEntity::new(Type::Spoiler, offset, length);
        let pre = |offset, length| MessageEntity::new(Type::Pre, offset, length);
        let pre_code = |offset, length, language: &str| {
            MessageEntity::with_argument(Type::PreCode, offset, length, language.to_owned())
        };
        let text_url = |offset, length, url: &str| {
            MessageEntity::with_argument(Type::TextUrl, offset, length, url.to_owned())
        };

        // v3 delimiters are doubled; single-character pairs stay literal.
        assert_eq!(parse("__i__"), ("i".to_owned(), vec![italic(0, 1)]));
        assert_eq!(parse("**b**"), ("b".to_owned(), vec![bold(0, 1)]));
        assert_eq!(parse("~~s~~"), ("s".to_owned(), vec![strike(0, 1)]));
        assert_eq!(parse("||p||"), ("p".to_owned(), vec![spoiler(0, 1)]));
        assert_eq!(
            parse("**bold _italic_ bold**"),
            ("bold _italic_ bold".to_owned(), vec![bold(0, 18)])
        );

        // Mixed battery: entities at stripped-text offsets.
        assert_eq!(
            parse("__i__ ~~s~~ **b** ||sp|| [t](https://t.me/x)"),
            (
                "i s b sp t".to_owned(),
                vec![
                    italic(0, 1),
                    strike(2, 1),
                    bold(4, 1),
                    spoiler(6, 2),
                    text_url(9, 1, "https://t.me/x"),
                ]
            )
        );
        assert_eq!(
            parse("**a** and **b**"),
            ("a and b".to_owned(), vec![bold(0, 1), bold(6, 1)])
        );
        // v3 never errors: lone and unpaired delimiters stay literal.
        assert_eq!(parse("__"), ("__".to_owned(), Vec::new()));
        assert_eq!(parse("**"), ("**".to_owned(), Vec::new()));

        // Pre entities: the language is recognized only before a newline;
        // without one the whole fence body is the content.
        assert_eq!(
            parse("```python\nprint(1)\n```"),
            ("print(1)\n".to_owned(), vec![pre_code(0, 9, "python")])
        );
        assert_eq!(
            parse("```lang code```"),
            ("lang code".to_owned(), vec![pre(0, 9)])
        );
        // CRLF after a fence is not a language separator: it stays content.
        assert_eq!(parse("```\rx\r```"), ("\rx\r".to_owned(), vec![pre(0, 3)]));
        // Double backticks are not fences.
        assert_eq!(parse("a `` b"), ("a `` b".to_owned(), Vec::new()));

        // UTF-16 offsets: astral emoji count as 2, CJK as 1 each.
        assert_eq!(
            parse("\u{1f600}**b**\u{1f400}"),
            ("\u{1f600}b\u{1f400}".to_owned(), vec![bold(2, 1)])
        );
        assert_eq!(
            parse("中文**粗体**"),
            ("中文粗体".to_owned(), vec![bold(2, 2)])
        );

        // Predetermined splittable entities win over the syntax they cover;
        // new entities are parsed in the gaps.
        assert_eq!(
            parse_with("keep **new**", vec![bold(0, 4)]),
            ("keep new".to_owned(), vec![bold(0, 4), bold(5, 3)])
        );
        // The pair inside the predetermined entity is still found: the C++
        // `combine` is a plain append, so the surviving entities are
        // emitted twice here (the API path's fix_entities merges the
        // duplicates afterwards).
        assert_eq!(
            parse_with("~~a~~ __b__", vec![strike(0, 3)]),
            (
                "a b".to_owned(),
                vec![strike(0, 1), strike(0, 1), italic(2, 1)]
            )
        );
        // A predetermined Pre entity keeps its fence; the tail is parsed.
        assert_eq!(
            parse_with("```x``` **b**", vec![pre(0, 3)]),
            ("```x``` b".to_owned(), vec![pre(0, 3), bold(8, 1)])
        );
    }

    /// M.8.8 hand-traced vectors for `get_markdown_v3`: the formatting of
    /// every supported type, the kept-entity paths (underline, blockquote,
    /// invalid language codes) and the parse-check return-original
    /// behavior.
    #[test]
    fn get_markdown_v3_golden_vectors() {
        fn get(text: &str, entities: Vec<MessageEntity>) -> (String, Vec<MessageEntity>) {
            let result = get_markdown_v3(FormattedText {
                text: text.to_owned(),
                entities,
            });
            (result.text, result.entities)
        }
        let bold = |offset, length| MessageEntity::new(Type::Bold, offset, length);
        let italic = |offset, length| MessageEntity::new(Type::Italic, offset, length);
        let strike = |offset, length| MessageEntity::new(Type::Strikethrough, offset, length);
        let spoiler = |offset, length| MessageEntity::new(Type::Spoiler, offset, length);
        let code = |offset, length| MessageEntity::new(Type::Code, offset, length);
        let pre = |offset, length| MessageEntity::new(Type::Pre, offset, length);
        let pre_code = |offset, length, language: &str| {
            MessageEntity::with_argument(Type::PreCode, offset, length, language.to_owned())
        };
        let text_url = |offset, length, url: &str| {
            MessageEntity::with_argument(Type::TextUrl, offset, length, url.to_owned())
        };
        let underline = |offset, length| MessageEntity::new(Type::Underline, offset, length);

        // The four splittable types and TextUrl format and drop entities.
        assert_eq!(
            get(
                "italic bold strike spoiler",
                vec![italic(0, 6), bold(7, 4), strike(12, 6), spoiler(19, 7)]
            ),
            (
                "__italic__ **bold** ~~strike~~ ||spoiler||".to_owned(),
                Vec::new()
            )
        );
        assert_eq!(
            get("url", vec![text_url(0, 3, "https://t.me/x")]),
            ("[url](https://t.me/x)".to_owned(), Vec::new())
        );

        // Code/Pre/PreCode fences; PreCode needs a newline after the fence.
        assert_eq!(
            get("code", vec![code(0, 4)]),
            ("`code`".to_owned(), Vec::new())
        );
        assert_eq!(
            get("x", vec![pre(0, 1)]),
            ("```\nx```".to_owned(), Vec::new())
        );
        assert_eq!(
            get("line1\nline2", vec![pre(0, 11)]),
            ("```\nline1\nline2```".to_owned(), Vec::new())
        );
        assert_eq!(
            get("print(1)\n", vec![pre_code(0, 9, "python")]),
            ("```python\nprint(1)\n```".to_owned(), Vec::new())
        );

        // Unsupported forms: the entity is kept and the text stays as-is.
        assert_eq!(
            get("a b", vec![pre_code(0, 3, "c x")]),
            ("a b".to_owned(), vec![pre_code(0, 3, "c x")])
        );
        assert_eq!(
            get("ab", vec![underline(0, 2)]),
            ("ab".to_owned(), vec![underline(0, 2)])
        );
        assert_eq!(
            get("x", vec![MessageEntity::new(Type::BlockQuote, 0, 1)]),
            (
                "x".to_owned(),
                vec![MessageEntity::new(Type::BlockQuote, 0, 1)]
            )
        );

        // A code entity containing a backtick cannot round-trip (the
        // formatted fence would not re-parse to the same entity), so the
        // original text and entities are returned.
        assert_eq!(
            get("a ` b", vec![code(2, 1)]),
            ("a ` b".to_owned(), vec![code(2, 1)])
        );
        // Nested Bold+Italic round-trips through the doubled delimiters
        // (the parse-check passes, so the formatted text is returned).
        assert_eq!(
            get("nested", vec![bold(0, 6), italic(1, 2)]),
            ("**n__es__ted**".to_owned(), Vec::new())
        );

        // Adjacent entities round-trip and drop out.
        assert_eq!(
            get("abcd", vec![bold(0, 2), italic(2, 2)]),
            ("**ab**__cd__".to_owned(), Vec::new())
        );

        // UTF-16 offsets: the astral emoji counts as 2 code units.
        assert_eq!(
            get("emoji \u{1f600}", vec![italic(0, 5)]),
            ("__emoji__ \u{1f600}".to_owned(), Vec::new())
        );
    }

    /// M.8.8 parse→format→parse round-trip fixed points: for every corpus
    /// text, `get_markdown_v3` output re-parses to the exact entities
    /// `parse_markdown_v3` produced (canonical formatting is idempotent).
    #[test]
    fn markdown_v3_round_trip_fixed_points() {
        let corpus = [
            "__i__ ~~s~~ **b** ||sp|| [t](https://t.me/x)",
            "**bold _italic_ bold**",
            "```python\nprint(1)\n```",
            "```\r\nwin\r\n```",
            "`code` mid **bold** end",
            "**a** and **b**",
            "**a**__b__**c**",
            "||p\np||",
            "```lang code```",
            "[t](https://t.me/x) [u](https://t.me/y)",
            "\u{1f600}**b**\u{1f400}",
            "中文**粗体**",
            ">quoted **b**",
            "**b** >x",
            "just plain text",
            "",
        ];
        for text in corpus {
            let parsed = parse_markdown_v3(FormattedText {
                text: text.to_owned(),
                entities: Vec::new(),
            });
            let markdown = get_markdown_v3(parsed.clone());
            let reparsed = parse_markdown_v3(markdown);
            assert_eq!(reparsed, parsed, "round-trip fixed point for {text:?}");
        }
    }

    // ===== M.8.9 — HTML parser (MessageEntity.cpp:3289-3578) =====

    /// M.8.9 hand-traced vectors for `parse_html` on the direct function,
    /// covering the tag aliases, the attribute subset, entity decoding,
    /// pre/code merging, UTF-16 offsets and the 400 error paths. The
    /// `td_execute` oracle corpus for the same dialect (including the
    /// malformed-HTML fuzz seeds) is the last 612 entries of
    /// `testdata/message_entity_cpp_golden.jsonl`.
    #[test]
    fn parse_html_golden_vectors() {
        fn parse(text: &str) -> Result<(String, Vec<MessageEntity>), String> {
            let mut text = text.to_owned();
            let entities = parse_html(&mut text).map_err(|status| status.message().to_owned())?;
            Ok((text, entities))
        }
        let entity =
            |r#type: Type, offset: i32, length: i32| MessageEntity::new(r#type, offset, length);

        // Text without tags (or with only unknown entities) is unchanged.
        assert_eq!(parse(""), Ok(("".to_owned(), Vec::new())));
        assert_eq!(parse("➡️ ➡️"), Ok(("➡️ ➡️".to_owned(), Vec::new())));
        assert_eq!(
            parse("&ge;&lt;&gt;&amp;&quot;&laquo;&raquo;&#12345678;"),
            Ok(("&ge;<>&\"&laquo;&raquo;&#12345678;".to_owned(), Vec::new()))
        );

        // The four decoded entities and the numeric forms.
        assert_eq!(
            parse("&lt;&gt;&amp;&quot;&#65;&#x1F600;"),
            Ok(("<>&\"A\u{1F600}".to_owned(), Vec::new()))
        );
        // An unterminated numeric entity overflows the C++ uint32
        // accumulator and is rejected without panicking (regression: the
        // accumulation uses wrapping arithmetic).
        assert_eq!(
            parse("&#99999999999999999999;"),
            Ok(("&#99999999999999999999;".to_owned(), Vec::new()))
        );

        // Tag aliases map to the same types.
        for (open, close) in [
            ("<i>", "</i>"),
            ("<em>", "</em>"),
            ("<b>", "</b>"),
            ("<strong>", "</strong>"),
            ("<u>", "</u>"),
            ("<ins>", "</ins>"),
            ("<s>", "</s>"),
            ("<strike>", "</strike>"),
            ("<del>", "</del>"),
        ] {
            let (open, close) = (open.to_owned(), close.to_owned());
            let r#type = match open.as_str() {
                "<i>" | "<em>" => Type::Italic,
                "<b>" | "<strong>" => Type::Bold,
                "<u>" | "<ins>" => Type::Underline,
                _ => Type::Strikethrough,
            };
            assert_eq!(
                parse(&format!("{open}x{close}")),
                Ok(("x".to_owned(), vec![entity(r#type, 0, 1)]))
            );
        }

        // UTF-16 offsets: an emoji pair counts 2 code units per emoji.
        assert_eq!(
            parse("🏟 🏟<i>🏟 &lt🏟</i>"),
            Ok(("🏟 🏟🏟 <🏟".to_owned(), vec![entity(Type::Italic, 5, 6)]))
        );
        assert_eq!(
            parse("🏟 🏟&lt;<i    aba  =  190azAz-.>a</i>"),
            Ok(("🏟 🏟<a".to_owned(), vec![entity(Type::Italic, 6, 1)]))
        );

        // Block quotes and the expandable flag (bare or with any value).
        assert_eq!(
            parse("<blockquote>x</blockquote>"),
            Ok(("x".to_owned(), vec![entity(Type::BlockQuote, 0, 1)]))
        );
        assert_eq!(
            parse("<blockquote expandable>x</blockquote>"),
            Ok((
                "x".to_owned(),
                vec![entity(Type::ExpandableBlockQuote, 0, 1)]
            ))
        );
        assert_eq!(
            parse("<blockquote expandable=false>x</blockquote>"),
            Ok((
                "x".to_owned(),
                vec![entity(Type::ExpandableBlockQuote, 0, 1)]
            ))
        );

        // Spoilers: the span class attribute and the dedicated tag.
        assert_eq!(
            parse("<span class = \"tg-spoiler\">x</span>"),
            Ok(("x".to_owned(), vec![entity(Type::Spoiler, 0, 1)]))
        );
        assert_eq!(
            parse("<tg-spoiler>x</tg-spoiler>"),
            Ok(("x".to_owned(), vec![entity(Type::Spoiler, 0, 1)]))
        );

        // pre/code and the PreCode merge in both nesting orders.
        assert_eq!(
            parse("<pre>x</pre>"),
            Ok(("x".to_owned(), vec![entity(Type::Pre, 0, 1)]))
        );
        assert_eq!(
            parse("<code>x</code>"),
            Ok(("x".to_owned(), vec![entity(Type::Code, 0, 1)]))
        );
        assert_eq!(
            parse("<pre><code>x</code></pre>"),
            Ok((
                "x".to_owned(),
                vec![entity(Type::Pre, 0, 1), entity(Type::Code, 0, 1)]
            ))
        );
        let pre_code = |offset, length, language: &str| {
            MessageEntity::with_argument(Type::PreCode, offset, length, language.to_owned())
        };
        assert_eq!(
            parse("<pre><code class=\"language-fift\">x</code></pre>"),
            Ok(("x".to_owned(), vec![pre_code(0, 1, "fift")]))
        );
        assert_eq!(
            parse("<code class=\"language-fift\"><pre>x</pre></code>"),
            Ok(("x".to_owned(), vec![pre_code(0, 1, "fift")]))
        );

        // Links: the href argument (fixed up by the link manager), the URL
        // taken from the link text, and rejected hrefs producing no entity.
        let text_url = |offset, length, url: &str| {
            MessageEntity::with_argument(Type::TextUrl, offset, length, url.to_owned())
        };
        assert_eq!(
            parse("<a href=telegram.org> </a>"),
            Ok((" ".to_owned(), vec![text_url(0, 1, "http://telegram.org/")]))
        );
        assert_eq!(
            parse("<a>telegram.org</a>"),
            Ok((
                "telegram.org".to_owned(),
                vec![text_url(0, 12, "http://telegram.org/")]
            ))
        );
        assert_eq!(parse("<a> </a>"), Ok((" ".to_owned(), Vec::new())));
        assert_eq!(
            parse("<a href=\"javascript:alert(1)\">x</a>"),
            Ok(("x".to_owned(), Vec::new()))
        );

        // Custom emoji and formatted date tags.
        assert_eq!(
            parse("<tg-emoji emoji-id = \"12345\">x</tg-emoji>"),
            Ok((
                "x".to_owned(),
                vec![MessageEntity::custom_emoji(0, 1, CustomEmojiId::new(12345))]
            ))
        );
        assert_eq!(
            parse("<tg-time unix = \"12345\", format = \"r\">x</tg-time>"),
            Ok((
                "x".to_owned(),
                vec![MessageEntity::formatted_date(
                    0,
                    1,
                    FormattedDate::from_raw(12345, 1)
                )]
            ))
        );
        // A tg-time without a format attribute still emits the entity; the
        // td_api layer cannot represent its null formatting type (the C++
        // passes nullptr there), which is why the corpus avoids the
        // serialization of this exact shape.
        let empty_format =
            parse("<tg-time unix=\"5\">x</tg-time>").expect("a tg-time without a format attribute");
        assert_eq!(empty_format.0, "x");
        assert_eq!(empty_format.1.len(), 1);
        assert_eq!(empty_format.1[0].r#type, Some(Type::FormattedDate));
        assert_eq!(empty_format.1[0].date.get_date(), 5);
        assert!(
            empty_format.1[0]
                .date
                .get_date_time_formatting_type_object()
                .is_none()
        );

        // Zero-length entities are dropped but the tags are consumed.
        assert_eq!(parse("<b></b>"), Ok(("".to_owned(), Vec::new())));
        // Empty end tags close the innermost entity.
        assert_eq!(
            parse("🏟 🏟&lt;<i>🏟 🏟&lt;</>"),
            Ok(("🏟 🏟<🏟 🏟<".to_owned(), vec![entity(Type::Italic, 6, 6)]))
        );
        // CR/LF/TAB survive inside entities (no clean_input_string here).
        assert_eq!(
            parse("<i>\r</i>"),
            Ok(("\r".to_owned(), vec![entity(Type::Italic, 0, 1)]))
        );

        // The 400 error paths with the exact C++ messages.
        assert_eq!(
            parse("<"),
            Err("Unclosed start tag at byte offset 0".to_owned())
        );
        assert_eq!(
            parse("</"),
            Err("Unexpected end tag at byte offset 0".to_owned())
        );
        assert_eq!(
            parse("<div>x</div>"),
            Err("Unsupported start tag \"div\" at byte offset 0".to_owned())
        );
        assert_eq!(
            parse("<b>x</i>"),
            Err("Unmatched end tag at byte offset 4, expected \"</b>\", found \"</i>\"".to_owned())
        );
        assert_eq!(
            parse("<b>x"),
            Err("Can't find end tag corresponding to start tag \"b\"".to_owned())
        );
        assert_eq!(
            parse("<span>x</span>"),
            Err("Tag \"span\" must have class \"tg-spoiler\" at byte offset 0".to_owned())
        );
        assert_eq!(
            parse("<tg-emoji>x</tg-emoji>"),
            Err("Invalid custom emoji identifier specified".to_owned())
        );
        assert_eq!(
            parse("<tg-time unix=\"5\" format=\"z\">x</tg-time>"),
            Err("Invalid date format used".to_owned())
        );
        assert_eq!(
            parse("<i    =aba>"),
            Err("Empty attribute name in the tag \"i\" at byte offset 0".to_owned())
        );
        // Decoded surrogate halves fail the final UTF-8 recheck.
        assert_eq!(
            parse("&#xDFDF;"),
            Err("Text contains invalid Unicode characters after decoding HTML entities, check for unmatched surrogate code units".to_owned())
        );
    }

    // ===== M.8.2 — input converters + fix_entities =====

    /// td_api::FormattedText → FormattedText (`get_formatted_text_of_input`)
    /// → td_api::FormattedText (`get_formatted_text_object`) round trip on
    /// a corpus covering every td_api entity type. A user dialog on the
    /// `()` manager skips bot-command discovery, so the corpus entity set
    /// survives the merge with the finders.
    #[test]
    fn formatted_text_input_round_trip() {
        fn td_entity(
            offset: i32,
            length: i32,
            r#type: td_api::TextEntityType,
        ) -> td_api::TextEntity {
            td_api::TextEntity {
                offset,
                length,
                r#type: Some(r#type),
            }
        }
        let bold = || td_api::TextEntityType::textEntityTypeBold(td_api::textEntityTypeBold {});
        let italic =
            || td_api::TextEntityType::textEntityTypeItalic(td_api::textEntityTypeItalic {});
        let code = || td_api::TextEntityType::textEntityTypeCode(td_api::textEntityTypeCode {});

        // (input, expected fixed FormattedText, expected td_api output when
        // it differs from the input).
        let cases: Vec<(
            td_api::FormattedText,
            FormattedText,
            Option<td_api::FormattedText>,
        )> = vec![
            // Simple per-type cases: the old entity wins the merge.
            (
                td_api::FormattedText {
                    text: "@abc".into(),
                    entities: vec![td_entity(
                        0,
                        4,
                        td_api::TextEntityType::textEntityTypeMention(
                            td_api::textEntityTypeMention {},
                        ),
                    )],
                },
                FormattedText {
                    text: "@abc".into(),
                    entities: vec![entity(Type::Mention, 0, 4)],
                },
                None,
            ),
            (
                td_api::FormattedText {
                    text: "#tag".into(),
                    entities: vec![td_entity(
                        0,
                        4,
                        td_api::TextEntityType::textEntityTypeHashtag(
                            td_api::textEntityTypeHashtag {},
                        ),
                    )],
                },
                FormattedText {
                    text: "#tag".into(),
                    entities: vec![entity(Type::Hashtag, 0, 4)],
                },
                None,
            ),
            (
                td_api::FormattedText {
                    text: "/cmd@bot".into(),
                    entities: vec![td_entity(
                        0,
                        8,
                        td_api::TextEntityType::textEntityTypeBotCommand(
                            td_api::textEntityTypeBotCommand {},
                        ),
                    )],
                },
                FormattedText {
                    text: "/cmd@bot".into(),
                    entities: vec![entity(Type::BotCommand, 0, 8)],
                },
                None,
            ),
            (
                td_api::FormattedText {
                    text: "example.com".into(),
                    entities: vec![td_entity(
                        0,
                        11,
                        td_api::TextEntityType::textEntityTypeUrl(td_api::textEntityTypeUrl {}),
                    )],
                },
                FormattedText {
                    text: "example.com".into(),
                    entities: vec![entity(Type::Url, 0, 11)],
                },
                None,
            ),
            (
                td_api::FormattedText {
                    text: "a@b.c".into(),
                    entities: vec![td_entity(
                        0,
                        5,
                        td_api::TextEntityType::textEntityTypeEmailAddress(
                            td_api::textEntityTypeEmailAddress {},
                        ),
                    )],
                },
                FormattedText {
                    text: "a@b.c".into(),
                    entities: vec![entity(Type::EmailAddress, 0, 5)],
                },
                None,
            ),
            (
                td_api::FormattedText {
                    text: "$ABC".into(),
                    entities: vec![td_entity(
                        0,
                        4,
                        td_api::TextEntityType::textEntityTypeCashtag(
                            td_api::textEntityTypeCashtag {},
                        ),
                    )],
                },
                FormattedText {
                    text: "$ABC".into(),
                    entities: vec![entity(Type::Cashtag, 0, 4)],
                },
                None,
            ),
            (
                td_api::FormattedText {
                    text: "+15555550100".into(),
                    entities: vec![td_entity(
                        0,
                        12,
                        td_api::TextEntityType::textEntityTypePhoneNumber(
                            td_api::textEntityTypePhoneNumber {},
                        ),
                    )],
                },
                FormattedText {
                    text: "+15555550100".into(),
                    entities: vec![entity(Type::PhoneNumber, 0, 12)],
                },
                None,
            ),
            (
                td_api::FormattedText {
                    text: "4111111111111111".into(),
                    entities: vec![td_entity(
                        0,
                        16,
                        td_api::TextEntityType::textEntityTypeBankCardNumber(
                            td_api::textEntityTypeBankCardNumber {},
                        ),
                    )],
                },
                FormattedText {
                    text: "4111111111111111".into(),
                    entities: vec![entity(Type::BankCardNumber, 0, 16)],
                },
                None,
            ),
            (
                td_api::FormattedText {
                    text: "abc".into(),
                    entities: vec![td_entity(0, 3, bold())],
                },
                FormattedText {
                    text: "abc".into(),
                    entities: vec![entity(Type::Bold, 0, 3)],
                },
                None,
            ),
            (
                td_api::FormattedText {
                    text: "abc".into(),
                    entities: vec![td_entity(0, 3, italic())],
                },
                FormattedText {
                    text: "abc".into(),
                    entities: vec![entity(Type::Italic, 0, 3)],
                },
                None,
            ),
            (
                td_api::FormattedText {
                    text: "abc".into(),
                    entities: vec![td_entity(
                        0,
                        3,
                        td_api::TextEntityType::textEntityTypeUnderline(
                            td_api::textEntityTypeUnderline {},
                        ),
                    )],
                },
                FormattedText {
                    text: "abc".into(),
                    entities: vec![entity(Type::Underline, 0, 3)],
                },
                None,
            ),
            (
                td_api::FormattedText {
                    text: "abc".into(),
                    entities: vec![td_entity(
                        0,
                        3,
                        td_api::TextEntityType::textEntityTypeStrikethrough(
                            td_api::textEntityTypeStrikethrough {},
                        ),
                    )],
                },
                FormattedText {
                    text: "abc".into(),
                    entities: vec![entity(Type::Strikethrough, 0, 3)],
                },
                None,
            ),
            (
                td_api::FormattedText {
                    text: "abc".into(),
                    entities: vec![td_entity(
                        0,
                        3,
                        td_api::TextEntityType::textEntityTypeSpoiler(
                            td_api::textEntityTypeSpoiler {},
                        ),
                    )],
                },
                FormattedText {
                    text: "abc".into(),
                    entities: vec![entity(Type::Spoiler, 0, 3)],
                },
                None,
            ),
            (
                td_api::FormattedText {
                    text: "abc".into(),
                    entities: vec![td_entity(
                        0,
                        3,
                        td_api::TextEntityType::textEntityTypeBlockQuote(
                            td_api::textEntityTypeBlockQuote {},
                        ),
                    )],
                },
                FormattedText {
                    text: "abc".into(),
                    entities: vec![entity(Type::BlockQuote, 0, 3)],
                },
                None,
            ),
            (
                td_api::FormattedText {
                    text: "abc".into(),
                    entities: vec![td_entity(
                        0,
                        3,
                        td_api::TextEntityType::textEntityTypeExpandableBlockQuote(
                            td_api::textEntityTypeExpandableBlockQuote {},
                        ),
                    )],
                },
                FormattedText {
                    text: "abc".into(),
                    entities: vec![entity(Type::ExpandableBlockQuote, 0, 3)],
                },
                None,
            ),
            (
                td_api::FormattedText {
                    text: "abc".into(),
                    entities: vec![td_entity(0, 3, code())],
                },
                FormattedText {
                    text: "abc".into(),
                    entities: vec![entity(Type::Code, 0, 3)],
                },
                None,
            ),
            (
                td_api::FormattedText {
                    text: "abc".into(),
                    entities: vec![td_entity(
                        0,
                        3,
                        td_api::TextEntityType::textEntityTypePre(td_api::textEntityTypePre {}),
                    )],
                },
                FormattedText {
                    text: "abc".into(),
                    entities: vec![entity(Type::Pre, 0, 3)],
                },
                None,
            ),
            (
                td_api::FormattedText {
                    text: "abc".into(),
                    entities: vec![td_entity(
                        0,
                        3,
                        td_api::TextEntityType::textEntityTypePreCode(
                            td_api::textEntityTypePreCode {
                                language: "python".into(),
                            },
                        ),
                    )],
                },
                FormattedText {
                    text: "abc".into(),
                    entities: vec![MessageEntity::with_argument(
                        Type::PreCode,
                        0,
                        3,
                        "python".to_owned(),
                    )],
                },
                None,
            ),
            (
                td_api::FormattedText {
                    text: "abc".into(),
                    entities: vec![td_entity(
                        0,
                        3,
                        td_api::TextEntityType::textEntityTypeTextUrl(
                            td_api::textEntityTypeTextUrl {
                                url: "https://t.me/".into(),
                            },
                        ),
                    )],
                },
                FormattedText {
                    text: "abc".into(),
                    entities: vec![MessageEntity::with_argument(
                        Type::TextUrl,
                        0,
                        3,
                        "https://t.me/".to_owned(),
                    )],
                },
                None,
            ),
            (
                td_api::FormattedText {
                    text: "abc".into(),
                    entities: vec![td_entity(
                        0,
                        3,
                        td_api::TextEntityType::textEntityTypeMentionName(
                            td_api::textEntityTypeMentionName { user_id: 777 },
                        ),
                    )],
                },
                FormattedText {
                    text: "abc".into(),
                    entities: vec![MessageEntity::mention_name(0, 3, UserId::new(777))],
                },
                None,
            ),
            (
                td_api::FormattedText {
                    text: "1:23".into(),
                    entities: vec![td_entity(
                        0,
                        4,
                        td_api::TextEntityType::textEntityTypeMediaTimestamp(
                            td_api::textEntityTypeMediaTimestamp {
                                media_timestamp: 83,
                            },
                        ),
                    )],
                },
                FormattedText {
                    text: "1:23".into(),
                    entities: vec![MessageEntity::media_timestamp(0, 4, 83)],
                },
                None,
            ),
            (
                td_api::FormattedText {
                    text: "😀".into(),
                    entities: vec![td_entity(
                        0,
                        2,
                        td_api::TextEntityType::textEntityTypeCustomEmoji(
                            td_api::textEntityTypeCustomEmoji { custom_emoji_id: 1 },
                        ),
                    )],
                },
                FormattedText {
                    text: "😀".into(),
                    entities: vec![MessageEntity::custom_emoji(0, 2, CustomEmojiId::new(1))],
                },
                None,
            ),
            (
                td_api::FormattedText {
                    text: "abc".into(),
                    entities: vec![td_entity(
                        0,
                        3,
                        td_api::TextEntityType::textEntityTypeDateTime(
                            td_api::textEntityTypeDateTime {
                                unix_time: 1_000_000_000,
                                formatting_type: Some(
                                    td_api::DateTimeFormattingType::dateTimeFormattingTypeRelative(
                                        td_api::dateTimeFormattingTypeRelative {},
                                    ),
                                ),
                            },
                        ),
                    )],
                },
                FormattedText {
                    text: "abc".into(),
                    entities: vec![MessageEntity::formatted_date(
                        0,
                        3,
                        FormattedDate::from_raw(1_000_000_000, date_flags::RELATIVE),
                    )],
                },
                None,
            ),
            // Overlapping splittable entities are resplit by fix_entities:
            // the common range is split per type into non-overlapping
            // Bold/Italic runs.
            (
                td_api::FormattedText {
                    text: "abcdefghi".into(),
                    entities: vec![td_entity(0, 5, bold()), td_entity(3, 6, italic())],
                },
                FormattedText {
                    text: "abcdefghi".into(),
                    entities: vec![
                        entity(Type::Bold, 0, 3),
                        entity(Type::Italic, 3, 6),
                        entity(Type::Bold, 3, 2),
                    ],
                },
                Some(td_api::FormattedText {
                    text: "abcdefghi".into(),
                    entities: vec![
                        td_entity(0, 3, bold()),
                        td_entity(3, 6, italic()),
                        td_entity(3, 2, bold()),
                    ],
                }),
            ),
            // Intersecting continuous entities keep the first one.
            (
                td_api::FormattedText {
                    text: "abcdefg".into(),
                    entities: vec![
                        td_entity(0, 4, code()),
                        td_entity(
                            2,
                            5,
                            td_api::TextEntityType::textEntityTypeTextUrl(
                                td_api::textEntityTypeTextUrl {
                                    url: "https://t.me/".into(),
                                },
                            ),
                        ),
                    ],
                },
                FormattedText {
                    text: "abcdefg".into(),
                    entities: vec![entity(Type::Code, 0, 4)],
                },
                Some(td_api::FormattedText {
                    text: "abcdefg".into(),
                    entities: vec![td_entity(0, 4, code())],
                }),
            ),
            // A blockquote may contain other entities.
            (
                td_api::FormattedText {
                    text: "abcde".into(),
                    entities: vec![
                        td_entity(
                            0,
                            5,
                            td_api::TextEntityType::textEntityTypeBlockQuote(
                                td_api::textEntityTypeBlockQuote {},
                            ),
                        ),
                        td_entity(1, 3, bold()),
                    ],
                },
                FormattedText {
                    text: "abcde".into(),
                    entities: vec![entity(Type::BlockQuote, 0, 5), entity(Type::Bold, 1, 3)],
                },
                None,
            ),
            // '\r' is removed and the entity shrinks with it.
            (
                td_api::FormattedText {
                    text: "a\rb".into(),
                    entities: vec![td_entity(0, 3, bold())],
                },
                FormattedText {
                    text: "ab".into(),
                    entities: vec![entity(Type::Bold, 0, 2)],
                },
                Some(td_api::FormattedText {
                    text: "ab".into(),
                    entities: vec![td_entity(0, 2, bold())],
                }),
            ),
            // A user dialog skips bot-command discovery entirely.
            (
                td_api::FormattedText {
                    text: "/cmd@bot".into(),
                    entities: vec![],
                },
                FormattedText {
                    text: "/cmd@bot".into(),
                    entities: vec![],
                },
                None,
            ),
        ];
        for (input, expected, expected_obj) in cases {
            let out = get_formatted_text_of_input::<()>(
                Some(&mut ()),
                DialogId::User(UserId::new(123)),
                Some(input.clone()),
                false,
                false,
                false,
                false,
                None,
                false,
                0,
                None,
                true,
            )
            .unwrap();
            assert_eq!(out, expected, "fixed FormattedText for {input:?}");
            let obj = get_formatted_text_object(Some(&mut ()), &out, false, i32::MAX);
            assert_eq!(
                obj,
                expected_obj.unwrap_or(input),
                "td_api round trip for the case above"
            );
        }

        // Trim + ltrim_count.
        let mut ltrim_count = 0;
        let trimmed = get_formatted_text_of_input::<()>(
            Some(&mut ()),
            DialogId::User(UserId::new(123)),
            Some(td_api::FormattedText {
                text: "  abc".into(),
                entities: vec![td_entity(2, 3, bold())],
            }),
            false,
            false,
            false,
            false,
            Some(&mut ltrim_count),
            false,
            0,
            None,
            true,
        )
        .unwrap();
        assert_eq!(ltrim_count, 2);
        assert_eq!(
            trimmed,
            FormattedText {
                text: "abc".into(),
                entities: vec![entity(Type::Bold, 0, 3)],
            }
        );

        // Secret chat at layer 0 (`()` manager): underline, spoiler and
        // formatted date are dropped; bold and code stay.
        let secret_input = td_api::FormattedText {
            text: "abcde".into(),
            entities: vec![
                td_entity(0, 1, bold()),
                td_entity(
                    1,
                    1,
                    td_api::TextEntityType::textEntityTypeUnderline(
                        td_api::textEntityTypeUnderline {},
                    ),
                ),
                td_entity(
                    2,
                    1,
                    td_api::TextEntityType::textEntityTypeSpoiler(td_api::textEntityTypeSpoiler {}),
                ),
                td_entity(3, 1, code()),
                td_entity(
                    4,
                    1,
                    td_api::TextEntityType::textEntityTypeDateTime(
                        td_api::textEntityTypeDateTime {
                            unix_time: 1_000_000_000,
                            formatting_type: Some(
                                td_api::DateTimeFormattingType::dateTimeFormattingTypeRelative(
                                    td_api::dateTimeFormattingTypeRelative {},
                                ),
                            ),
                        },
                    ),
                ),
            ],
        };
        let secret_out = get_formatted_text_of_input::<()>(
            Some(&mut ()),
            DialogId::SecretChat(SecretChatId::new(1)),
            Some(secret_input),
            false,
            false,
            false,
            false,
            None,
            false,
            0,
            None,
            true,
        )
        .unwrap();
        assert_eq!(
            secret_out,
            FormattedText {
                text: "abcde".into(),
                entities: vec![entity(Type::Bold, 0, 1), entity(Type::Code, 3, 1)],
            }
        );

        // A dialog without premium custom emoji drops premium ones.
        let premium_check: &mut dyn FnMut(CustomEmojiId, bool) -> bool =
            &mut |custom_emoji_id, _remove_unknown| custom_emoji_id.get() == 1;
        let premium_out = get_formatted_text_of_input::<()>(
            Some(&mut ()),
            DialogId::User(UserId::new(123)),
            Some(td_api::FormattedText {
                text: "a😀b".into(),
                entities: vec![
                    td_entity(0, 1, bold()),
                    td_entity(
                        1,
                        2,
                        td_api::TextEntityType::textEntityTypeCustomEmoji(
                            td_api::textEntityTypeCustomEmoji { custom_emoji_id: 1 },
                        ),
                    ),
                ],
            }),
            false,
            false,
            false,
            false,
            None,
            false,
            0,
            Some(premium_check),
            false,
        )
        .unwrap();
        assert_eq!(
            premium_out,
            FormattedText {
                text: "a😀b".into(),
                entities: vec![entity(Type::Bold, 0, 1)],
            }
        );

        // always_parse_markdown runs parse_markdown_v3 and re-fixes.
        let markdown_out = get_formatted_text_of_input::<()>(
            Some(&mut ()),
            DialogId::User(UserId::new(123)),
            Some(td_api::FormattedText {
                text: "**bold**".into(),
                entities: vec![],
            }),
            false,
            false,
            false,
            false,
            None,
            true,
            0,
            None,
            true,
        )
        .unwrap();
        assert_eq!(
            markdown_out,
            FormattedText {
                text: "bold".into(),
                entities: vec![entity(Type::Bold, 0, 4)],
            }
        );

        // A null text is an error unless empty input is allowed.
        assert_eq!(
            get_formatted_text_of_input::<()>(
                None,
                DialogId::User(UserId::new(123)),
                None,
                false,
                false,
                false,
                false,
                None,
                false,
                0,
                None,
                true,
            )
            .unwrap_err()
            .message(),
            "Text must be non-empty"
        );
        assert_eq!(
            get_formatted_text_of_input::<()>(
                None,
                DialogId::User(UserId::new(123)),
                None,
                false,
                true,
                false,
                false,
                None,
                false,
                0,
                None,
                true,
            )
            .unwrap(),
            FormattedText::default()
        );
    }

    /// `get_formatted_text(user_manager, textWithEntities, ...)`
    /// (MessageEntity.cpp:4112-4120): the null wrapper and the delegation.
    #[test]
    fn formatted_text_of_text_with_entities_wrapper() {
        assert_eq!(
            get_formatted_text_of_text_with_entities::<()>(None, None, false, false, "test"),
            FormattedText::default()
        );
        let out = get_formatted_text_of_text_with_entities::<()>(
            None,
            Some(tdmtproto::telegram_api::TextWithEntities {
                text: "abc".into(),
                entities: vec![],
            }),
            false,
            false,
            "test",
        );
        assert_eq!(
            out,
            FormattedText {
                text: "abc".into(),
                entities: vec![],
            }
        );
    }

    /// `need_always_skip_bot_commands` (MessageEntity.cpp:4714-4740) with
    /// the `()` manager (no users, no secret chats, no bots).
    #[test]
    fn need_always_skip_bot_commands_rules() {
        // An invalid dialog always skips; a bot sender never skips.
        assert!(need_always_skip_bot_commands::<()>(
            None,
            DialogId::None,
            false
        ));
        assert!(!need_always_skip_bot_commands::<()>(
            None,
            DialogId::User(UserId::new(1)),
            true
        ));
        // The replies and verification-codes bots are always skipped.
        assert!(need_always_skip_bot_commands::<()>(
            None,
            DialogId::User(UserId::new(1271266957)),
            false
        ));
        assert!(need_always_skip_bot_commands::<()>(
            None,
            DialogId::User(UserId::new(489000)),
            false
        ));
        // Unknown users are not bots → skipped.
        assert!(need_always_skip_bot_commands::<()>(
            None,
            DialogId::User(UserId::new(123)),
            false
        ));
        // Secret chats with the `()` manager resolve to an invalid user.
        assert!(need_always_skip_bot_commands::<()>(
            None,
            DialogId::SecretChat(SecretChatId::new(1)),
            false
        ));
        // Chats and channels never skip bot commands.
        assert!(!need_always_skip_bot_commands::<()>(
            None,
            DialogId::Chat(ChatId::new(1)),
            false
        ));
        assert!(!need_always_skip_bot_commands::<()>(
            None,
            DialogId::Channel(ChannelId::new(1)),
            false
        ));
    }

    /// `remove_unallowed_quote_user_entities` (MessageEntity.cpp:4905-4910):
    /// found types survive, the rest are removed.
    #[test]
    fn quote_user_entity_rules() {
        let mut text = FormattedText {
            text: String::new(),
            entities: vec![
                entity(Type::Bold, 0, 1),
                entity(Type::Url, 1, 1),
                entity(Type::MediaTimestamp, 2, 1),
                entity(Type::BotCommand, 3, 1),
                entity(Type::Code, 4, 1),
            ],
        };
        // With both skip flags cleared every entity except Code is allowed.
        assert!(remove_unallowed_quote_user_entities(
            &mut text, false, false
        ));
        assert_eq!(
            text.entities.iter().map(|e| e.r#type).collect::<Vec<_>>(),
            vec![
                Some(Type::Bold),
                Some(Type::Url),
                Some(Type::MediaTimestamp),
                Some(Type::BotCommand),
            ]
        );
        // With both skip flags set the bot command and the media timestamp
        // go too; Url is a found type regardless of the flags.
        assert!(remove_unallowed_quote_user_entities(&mut text, true, true));
        assert_eq!(
            text.entities.iter().map(|e| e.r#type).collect::<Vec<_>>(),
            vec![Some(Type::Bold), Some(Type::Url)]
        );
    }

    /// `remove_premium_custom_emoji_entities` (MessageEntity.cpp:4860-4865)
    /// and `add_formatted_text_dependencies`
    /// (MessageEntity.cpp:4680-4687).
    #[test]
    fn premium_custom_emoji_and_dependencies() {
        let mut entities = vec![
            entity(Type::Bold, 0, 1),
            MessageEntity::custom_emoji(1, 1, CustomEmojiId::new(9)),
            MessageEntity::custom_emoji(2, 1, CustomEmojiId::new(10)),
        ];
        let check: &mut dyn FnMut(CustomEmojiId, bool) -> bool =
            &mut |custom_emoji_id, _remove_unknown| custom_emoji_id.get() == 9;
        remove_premium_custom_emoji_entities(&mut entities, true, Some(check));
        assert_eq!(
            entities,
            vec![
                entity(Type::Bold, 0, 1),
                MessageEntity::custom_emoji(2, 1, CustomEmojiId::new(10)),
            ]
        );
        // A cold cache with remove_unknown treats unknown emoji as premium.
        let mut entities = vec![MessageEntity::custom_emoji(0, 1, CustomEmojiId::new(9))];
        remove_premium_custom_emoji_entities(&mut entities, true, None);
        assert!(entities.is_empty());
        let mut entities = vec![MessageEntity::custom_emoji(0, 1, CustomEmojiId::new(9))];
        remove_premium_custom_emoji_entities(&mut entities, false, None);
        assert_eq!(entities.len(), 1);

        // Only the MentionName user id is collected (Dependencies::add
        // filters invalid ids).
        let text = FormattedText {
            text: String::new(),
            entities: vec![
                entity(Type::Bold, 0, 1),
                MessageEntity::mention_name(1, 2, UserId::new(777)),
            ],
        };
        let mut collected = Vec::new();
        add_formatted_text_dependencies(&mut |user_id| collected.push(user_id), Some(&text));
        assert_eq!(collected, vec![UserId::new(777)]);
        let mut collected = Vec::new();
        add_formatted_text_dependencies(&mut |user_id| collected.push(user_id), None);
        assert!(collected.is_empty());
    }

    // ----- M.8.5 fuzz-lite: entity fixup invariants (verify clause) ------
    // Random entity sets checked for the sortedness / non-intersection /
    // validity invariants of MessageEntity.cpp:1444-1827, against
    // independent references restated from the C++ in test code.

    /// A dependency-free SplitMix64: deterministic across runs and
    /// platforms (the crate has no rand dev-dependency).
    struct Prng(u64);

    impl Prng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    /// All 23 real entity types (the `Size` sentinel is not a variant).
    const ALL_TYPES: [Type; 23] = [
        Type::Mention,
        Type::Hashtag,
        Type::BotCommand,
        Type::Url,
        Type::EmailAddress,
        Type::Bold,
        Type::Italic,
        Type::Code,
        Type::Pre,
        Type::PreCode,
        Type::TextUrl,
        Type::MentionName,
        Type::Cashtag,
        Type::PhoneNumber,
        Type::Underline,
        Type::Strikethrough,
        Type::BlockQuote,
        Type::BankCardNumber,
        Type::MediaTimestamp,
        Type::Spoiler,
        Type::CustomEmoji,
        Type::ExpandableBlockQuote,
        Type::FormattedDate,
    ];

    /// Random text over 1-, 2-, 3- and 4-byte UTF-8 code points, with the
    /// byte offsets of every character boundary.
    struct TextFixture {
        text: String,
        boundaries: Vec<usize>,
    }

    fn random_text(prng: &mut Prng) -> TextFixture {
        const ALPHABET: [&str; 6] = ["a", " ", "\n", "é", "中", "🐀"];
        let mut text = String::new();
        let mut boundaries = vec![0usize];
        for _ in 0..3 + prng.below(24) {
            text.push_str(ALPHABET[prng.below(ALPHABET.len())]);
            boundaries.push(text.len());
        }
        TextFixture { text, boundaries }
    }

    /// A random entity spanning whole characters of `fixture`.
    fn random_entity(prng: &mut Prng, fixture: &TextFixture) -> MessageEntity {
        let begin = fixture.boundaries[prng.below(fixture.boundaries.len() - 1)];
        let end_candidates: Vec<usize> = fixture
            .boundaries
            .iter()
            .copied()
            .filter(|&end| end > begin)
            .collect();
        let end = end_candidates[prng.below(end_candidates.len())];
        MessageEntity::new(
            ALL_TYPES[prng.below(ALL_TYPES.len())],
            begin as i32,
            (end - begin) as i32,
        )
    }

    /// Sorted, non-intersecting entities built by construction (each
    /// entity starts after the previous one ends).
    fn random_non_intersecting_set(prng: &mut Prng, fixture: &TextFixture) -> Vec<MessageEntity> {
        let mut entities = Vec::new();
        let mut occupied_until = 0usize;
        for _ in 0..prng.below(8) {
            let candidates: Vec<usize> = (0..fixture.boundaries.len() - 1)
                .filter(|&i| fixture.boundaries[i] >= occupied_until)
                .collect();
            if candidates.is_empty() {
                break;
            }
            let begin = fixture.boundaries[candidates[prng.below(candidates.len())]];
            let end_candidates: Vec<usize> = fixture
                .boundaries
                .iter()
                .copied()
                .filter(|&end| end > begin)
                .collect();
            let end = end_candidates[prng.below(end_candidates.len())];
            entities.push(MessageEntity::new(
                ALL_TYPES[prng.below(ALL_TYPES.len())],
                begin as i32,
                (end - begin) as i32,
            ));
            occupied_until = end;
        }
        entities
    }

    fn check_sorted_non_intersecting(entities: &[MessageEntity]) {
        for window in entities.windows(2) {
            assert!(window[0] <= window[1], "not sorted: {:?}", entities);
            assert!(
                window[0].offset + window[0].length <= window[1].offset,
                "intersecting: {:?}",
                entities
            );
        }
    }

    fn utf16_length(text: &str) -> i32 {
        text.chars().map(|c| c.len_utf16() as i32).sum()
    }

    /// Independent reference for `fix_entity_offsets`
    /// (MessageEntity.cpp:1679-1721): sort, greedily keep the first
    /// non-intersecting entities, then convert byte offsets to UTF-16
    /// code units via `char_indices`.
    fn reference_fix_entity_offsets(text: &str, entities: &mut Vec<MessageEntity>) {
        entities.sort();
        let mut kept: Vec<MessageEntity> = Vec::new();
        let mut last_entity_end = 0i32;
        for entity in entities.drain(..) {
            if entity.offset >= last_entity_end {
                last_entity_end = entity.offset + entity.length;
                kept.push(entity);
            }
        }
        let byte_to_utf16 = |byte_offset: i32| -> i32 {
            let mut utf16_pos = 0i32;
            for (byte, c) in text.char_indices() {
                if byte as i32 >= byte_offset {
                    break;
                }
                utf16_pos += c.len_utf16() as i32;
            }
            utf16_pos
        };
        for entity in &mut kept {
            let begin = byte_to_utf16(entity.offset);
            let end = byte_to_utf16(entity.offset + entity.length);
            entity.offset = begin;
            entity.length = end - begin;
        }
        *entities = kept;
    }

    #[test]
    fn fix_entity_offsets_fuzz_lite() {
        let mut prng = Prng(0x6d38_2e35_91ab_c7d0);
        for _ in 0..3000 {
            let fixture = random_text(&mut prng);
            let input: Vec<MessageEntity> = (0..prng.below(12))
                .map(|_| random_entity(&mut prng, &fixture))
                .collect();

            let mut expected = input.clone();
            reference_fix_entity_offsets(&fixture.text, &mut expected);

            let mut actual = input.clone();
            fix_entity_offsets(&fixture.text, &mut actual);
            assert_eq!(
                actual, expected,
                "text {:?} input {:?}",
                fixture.text, input
            );

            check_sorted_non_intersecting(&actual);
            let utf16_len = utf16_length(&fixture.text);
            for entity in &actual {
                assert!(entity.offset >= 0);
                assert!(entity.offset + entity.length <= utf16_len);
            }
        }
    }

    #[test]
    fn remove_intersecting_entities_fuzz_lite() {
        let mut prng = Prng(0x51a3_f2c1_77de_0419);
        for _ in 0..3000 {
            let fixture = random_text(&mut prng);
            let mut input: Vec<MessageEntity> = (0..prng.below(12))
                .map(|_| random_entity(&mut prng, &fixture))
                .collect();
            input.sort();

            let mut expected: Vec<MessageEntity> = Vec::new();
            let mut last_entity_end = 0i32;
            for entity in input.iter().cloned() {
                if entity.offset >= last_entity_end {
                    last_entity_end = entity.offset + entity.length;
                    expected.push(entity);
                }
            }

            let mut actual = input.clone();
            remove_intersecting_entities(&mut actual);
            assert_eq!(actual, expected, "input {:?}", input);
            check_sorted_non_intersecting(&actual);
        }
    }

    #[test]
    fn merge_entities_fuzz_lite() {
        let mut prng = Prng(0x7a3c_55d9_e2b4_01c6);
        for _ in 0..3000 {
            let fixture = random_text(&mut prng);
            let old = random_non_intersecting_set(&mut prng, &fixture);
            let new = random_non_intersecting_set(&mut prng, &fixture);

            let actual = merge_entities(old.clone(), new.clone());
            check_sorted_non_intersecting(&actual);

            // Independent restatement: old entities survive; a new entity
            // survives unless it overlaps some old entity; the result is
            // their sorted union (both inputs are non-intersecting, so the
            // entity sets are unique).
            let mut expected = old.clone();
            'new_entities: for new_entity in &new {
                for old_entity in &old {
                    if new_entity.offset + new_entity.length > old_entity.offset
                        && new_entity.offset < old_entity.offset + old_entity.length
                    {
                        continue 'new_entities;
                    }
                }
                expected.push(new_entity.clone());
            }
            expected.sort();
            assert_eq!(actual, expected, "old {:?} new {:?}", old, new);
        }
    }

    /// Independent O(n²) restatement of `are_entities_valid`
    /// (MessageEntity.cpp:1559-1626): each entity is checked against
    /// every ancestor containing its start, and splittable entities of
    /// the same type may not touch.
    fn reference_entities_valid(entities: &[MessageEntity]) -> bool {
        if entities.is_empty() {
            return true;
        }
        let mut last_end_by_splittable = [-1i32; SPLITTABLE_ENTITY_TYPE_COUNT];
        for (index, entity) in entities.iter().enumerate() {
            let entity_end = entity.offset + entity.length;
            let mut ancestor_mask = 0i32;
            let mut parent: Option<&MessageEntity> = None;
            for prev in &entities[..index] {
                if entity.offset < prev.offset + prev.length {
                    ancestor_mask |= entity_type_mask(prev.r#type.unwrap());
                    parent = Some(prev);
                }
            }
            if let Some(parent) = parent {
                if entity_end > parent.offset + parent.length {
                    // entity intersects some previous entity
                    return false;
                }
                if (ancestor_mask & entity_type_mask(entity.r#type.unwrap())) != 0 {
                    // entity has the same type as one of the previous nested
                    return false;
                }
                if is_pre_entity(parent.r#type.unwrap()) {
                    // Pre and Code can't contain nested entities
                    return false;
                }
                if is_pre_entity(entity.r#type.unwrap())
                    && (ancestor_mask & !blockquote_entities_mask()) != 0
                {
                    // Pre and Code can't be part of other entities, except blockquote
                    return false;
                }
                if (is_continuous_entity(entity.r#type.unwrap())
                    || is_blockquote_entity(entity.r#type.unwrap()))
                    && (ancestor_mask & continuous_entities_mask()) != 0
                {
                    // continuous and blockquote can't be part of other continuous entity
                    return false;
                }
                if is_blockquote_entity(entity.r#type.unwrap())
                    && (ancestor_mask & blockquote_entities_mask()) != 0
                {
                    // blockquote entities can't be nested
                    return false;
                }
                if (ancestor_mask & splittable_entities_mask()) != 0 {
                    // the previous nested entity may be needed to split for consistency
                    return false;
                }
            }
            if is_splittable_entity(entity.r#type.unwrap()) {
                let splittable_index = splittable_entity_type_index(entity.r#type.unwrap());
                if last_end_by_splittable[splittable_index] >= entity.offset {
                    // the entities can be merged
                    return false;
                }
                last_end_by_splittable[splittable_index] = entity_end;
            }
        }
        true
    }

    /// Sorted, non-intersecting entities with random nesting: a parent
    /// spans a whole run of the flat entities underneath it (the parent
    /// type is biased toward an existing child type to hit same-type
    /// nesting).
    fn random_nested_set(prng: &mut Prng, fixture: &TextFixture) -> Vec<MessageEntity> {
        let mut entities = random_non_intersecting_set(prng, fixture);
        if !entities.is_empty() && prng.below(2) == 0 {
            let first = prng.below(entities.len());
            let last = first + prng.below(entities.len() - first);
            let parent_begin = entities[first].offset;
            let parent_end = entities[last].offset + entities[last].length;
            let parent_type = if prng.below(2) == 0 {
                entities[prng.below(entities.len())].r#type.unwrap()
            } else {
                ALL_TYPES[prng.below(ALL_TYPES.len())]
            };
            entities.push(MessageEntity::new(
                parent_type,
                parent_begin,
                parent_end - parent_begin,
            ));
            entities.sort();
        }
        entities
    }

    #[test]
    fn are_entities_valid_fuzz_lite() {
        let mut prng = Prng(0x3b7d_5a19_c04e_88f2);
        for _ in 0..4000 {
            let fixture = random_text(&mut prng);
            let entities = random_nested_set(&mut prng, &fixture);
            assert_eq!(
                are_entities_valid(&entities),
                reference_entities_valid(&entities),
                "entities {:?}",
                entities
            );
        }
    }

    #[test]
    fn remove_entities_intersecting_blockquote_fuzz_lite() {
        let mut prng = Prng(0x2b7c_91a4_f8d3_6e50);
        for _ in 0..3000 {
            let fixture = random_text(&mut prng);
            let input = random_non_intersecting_set(&mut prng, &fixture);
            // Half the entries are blockquote entities, half arbitrary
            // (the C++ skips non-blockquote entries while scanning).
            let blockquotes: Vec<MessageEntity> = random_non_intersecting_set(&mut prng, &fixture)
                .into_iter()
                .map(|mut blockquote| {
                    blockquote.r#type = Some(if prng.below(2) == 0 {
                        if prng.below(2) == 0 {
                            Type::BlockQuote
                        } else {
                            Type::ExpandableBlockQuote
                        }
                    } else {
                        ALL_TYPES[prng.below(ALL_TYPES.len())]
                    });
                    blockquote
                })
                .collect();

            // Independent restatement of MessageEntity.cpp:1653-1668:
            // the first blockquote entity extending past the entity start
            // drops it only when the entity crosses its boundary; an
            // entity fully inside a blockquote is kept.
            let expected: Vec<MessageEntity> = input
                .iter()
                .filter(|entity| {
                    let entity_end = entity.offset + entity.length;
                    let candidate = blockquotes.iter().find(|blockquote| {
                        is_blockquote_entity(blockquote.r#type.unwrap())
                            && blockquote.offset + blockquote.length > entity.offset
                    });
                    !candidate.is_some_and(|blockquote| {
                        blockquote.offset + blockquote.length < entity_end
                            || (entity.offset < blockquote.offset && blockquote.offset < entity_end)
                    })
                })
                .cloned()
                .collect();

            let mut actual = input.clone();
            remove_entities_intersecting_blockquote(&mut actual, &blockquotes);
            assert_eq!(
                actual, expected,
                "input {:?} blockquotes {:?}",
                input, blockquotes
            );
            check_sorted_non_intersecting(&actual);
        }
    }

    /// Directed cases for `remove_entities_intersecting_blockquote`
    /// (MessageEntity.cpp:1646-1677): an entity fully inside a
    /// blockquote is kept; an entity crossing a blockquote boundary is
    /// dropped; touching is not intersecting; the empty list is a fast
    /// path.
    #[test]
    fn remove_entities_intersecting_blockquote_directed() {
        let blockquote = [entity(Type::BlockQuote, 3, 7)];
        // crossing the left boundary: dropped
        let mut left = vec![entity(Type::Bold, 0, 5)];
        remove_entities_intersecting_blockquote(&mut left, &blockquote);
        assert!(left.is_empty());
        // fully inside: kept
        let mut inside = vec![entity(Type::Bold, 5, 3)];
        remove_entities_intersecting_blockquote(&mut inside, &blockquote);
        assert_eq!(inside, vec![entity(Type::Bold, 5, 3)]);
        // crossing the right boundary: dropped
        let mut right = vec![entity(Type::Bold, 8, 4)];
        remove_entities_intersecting_blockquote(&mut right, &blockquote);
        assert!(right.is_empty());
        // touching the blockquote end: kept
        let mut touching = vec![entity(Type::Bold, 10, 2)];
        remove_entities_intersecting_blockquote(&mut touching, &blockquote);
        assert_eq!(touching, vec![entity(Type::Bold, 10, 2)]);
        // non-blockquote entries in the list are skipped
        let mixed = [entity(Type::Bold, 0, 2), entity(Type::BlockQuote, 3, 7)];
        let mut crossing = vec![entity(Type::Bold, 8, 4)];
        remove_entities_intersecting_blockquote(&mut crossing, &mixed);
        assert!(crossing.is_empty());
        // empty blockquote list: everything survives
        let mut entities = vec![entity(Type::Bold, 1, 2), entity(Type::Url, 5, 2)];
        remove_entities_intersecting_blockquote(&mut entities, &[]);
        assert_eq!(entities.len(), 2);
    }

    /// `fix_entity_offsets` failure semantics: the C++ `CHECK(cnt == 0)`
    /// (MessageEntity.cpp:1719) is a fatal abort, ported as a panic.
    /// Offsets must land on UTF-8 character boundaries within the text;
    /// a 4-byte code point counts as two UTF-16 units.
    #[test]
    fn fix_entity_offsets_failure_semantics() {
        // entity begin in the middle of a multi-byte character
        let mut entities = vec![entity(Type::Bold, 1, 1)];
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                fix_entity_offsets("éé", &mut entities);
            }))
            .is_err()
        );
        // entity end in the middle of a multi-byte character
        let mut entities = vec![entity(Type::Bold, 0, 1)];
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                fix_entity_offsets("éé", &mut entities);
            }))
            .is_err()
        );
        // entity reaching past the end of the text
        let mut entities = vec![entity(Type::Bold, 0, 8)];
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                fix_entity_offsets("ab", &mut entities);
            }))
            .is_err()
        );
        // zero-length entity hits the CHECK in remove_intersecting_entities
        let mut entities = vec![entity(Type::Bold, 0, 0)];
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                fix_entity_offsets("ab", &mut entities);
            }))
            .is_err()
        );
        // a 4-byte code point becomes two UTF-16 code units
        let mut entities = vec![entity(Type::Bold, 0, 4), entity(Type::Url, 5, 1)];
        fix_entity_offsets("🐀ax", &mut entities);
        assert_eq!(
            entities,
            vec![entity(Type::Bold, 0, 2), entity(Type::Url, 3, 1)]
        );
    }

    /// `find_entities` output invariants over the matcher battery (the
    /// corpus replay covers value-level parity with the C++ oracle).
    #[test]
    fn find_entities_output_invariants() {
        for text in matcher_battery() {
            for skip_bot_commands in [false, true] {
                for skip_media_timestamps in [false, true] {
                    let entities = find_entities(&text, skip_bot_commands, skip_media_timestamps);
                    check_sorted_non_intersecting(&entities);
                    let utf16_len = utf16_length(&text);
                    for entity in &entities {
                        assert!(entity.offset >= 0, "{} {:?}", text, entities);
                        assert!(
                            entity.offset + entity.length <= utf16_len,
                            "{} {:?}",
                            text,
                            entities
                        );
                    }
                }
            }
        }
    }
}
