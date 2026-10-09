//! Port of `td/telegram/ChatManager.{h,cpp}` (Track I, M.5): the basic-group
//! (`Chat`/`ChatFull`, `ChatId`) and channel/supergroup (`Channel`/`ChannelFull`,
//! `ChannelId`) cache. This file opens the slice with the id newtypes and the
//! header enums the records are keyed and flagged by (`ChatId.h`, `ChannelId.h`,
//! `ChannelType.h`, `PublicDialogType.h`) plus the id stand-ins the record
//! shapes drag in.
//!
//! Id conventions, matching the M.4 user-manager ids:
//! - persistence inlines the raw integer at the record store/parse call sites
//!   (the C++ `ChatId`/`ChannelId` header `store`/`parse` — `store_long` with
//!   the `Support64BitIds`-gated fetch — materialize inside the M.5.8+ record
//!   framings, exactly like `UserId` in the user records);
//! - `MessageId` (td-side, `MessageId.h`) is an opaque i64 stand-in: the
//!   server/local/scheduled bit layout, `is_valid` and the `MessageType` split
//!   land with the MessagesManager step. Distinct from `tdmtproto::MessageId`,
//!   the MTProto-side id;
//! - `DcId` (`net/DcId.h`), `FileSourceId` (`files/FileSourceId.h`) and
//!   `StickerSetId` (`StickerSetId.h`) port the value semantics their C++
//!   headers define; `StickerSetId`'s log-event store/parse (StickerSetId.cpp)
//!   inlines at the ChannelFull call sites;
//! - `get_public_dialog_type` (PublicDialogType.h) defers to the
//!   created-public-dialogs step — its only consumer — because it converts the
//!   td_api `PublicChatType` surface.

use std::collections::{BTreeMap, BTreeSet};

use crate::emoji_status::{CustomEmojiId, EmojiStatus};
use crate::global::Global;
use crate::misc::strip_empty_characters;
use crate::peer_color::AccentColorId;
use crate::peer_color::PeerColor;
use crate::photo::{DialogPhoto, Photo, get_chat_photo_object, photo_get_file_ids};
use crate::restriction_reason::{
    RestrictionOptions, RestrictionReason, get_restriction_info_object, get_restriction_reasons,
    get_restriction_reasons_from_legacy, parse_restriction_reasons, store_restriction_reasons,
};
use crate::store_parse::{
    Parser, store_f64, store_flags, store_i32, store_i64, store_string, store_u32, store_u64,
    version,
};
use crate::td_api;
use crate::user_manager::{
    AccessRights, CommunityId, FileId, SecretChatId, StoryId, TimeCtx, UserEnv, UserId, UserManager,
};
use crate::usernames::Usernames;
use tdmtproto::telegram_api;
use tdutils::promise::Promise;
use tdutils::status::Status;

/// `td::ChatId` (ChatId.h): `0 < id ≤ MAX_CHAT_ID`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChatId(i64);

impl ChatId {
    pub const MAX_CHAT_ID: i64 = 999_999_999_999;

    pub fn new(id: i64) -> ChatId {
        ChatId(id)
    }

    pub fn is_valid(self) -> bool {
        0 < self.0 && self.0 <= Self::MAX_CHAT_ID
    }

    pub fn get(self) -> i64 {
        self.0
    }
}

/// `td::ChannelId` (ChannelId.h). One id space for supergroups and
/// broadcasts, with the regular band half-open below `MAX_CHANNEL_ID` (the
/// last `(1 << 31) - 1` identifiers below it are reserved for secret-chat
/// dialog identifiers) and the monoforum band `[MIN_MONOFORUM_CHANNEL_ID,
/// MAX_MONOFORUM_CHANNEL_ID)` above 10¹².
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChannelId(i64);

impl ChannelId {
    pub const MAX_CHANNEL_ID: i64 = 1_000_000_000_000 - (1i64 << 31);
    pub const MIN_MONOFORUM_CHANNEL_ID: i64 = 1_000_000_000_000 + (1i64 << 31) + 1;
    pub const MAX_MONOFORUM_CHANNEL_ID: i64 = 3_000_000_000_000;

    pub fn new(id: i64) -> ChannelId {
        ChannelId(id)
    }

    pub fn is_valid(self) -> bool {
        self.is_regular_channel()
            || (Self::MIN_MONOFORUM_CHANNEL_ID <= self.0 && self.0 < Self::MAX_MONOFORUM_CHANNEL_ID)
    }

    pub fn is_regular_channel(self) -> bool {
        0 < self.0 && self.0 < Self::MAX_CHANNEL_ID
    }

    pub fn get(self) -> i64 {
        self.0
    }

    /// `ChannelId::store` (ChannelId.h:56-58) — the raw id as int64.
    pub fn store(self, out: &mut Vec<u8>) {
        crate::store_parse::store_i64(out, self.0);
    }

    /// `ChannelId::parse` (ChannelId.h:60-67) — int64 from
    /// `Version::Support64BitIds` (33) on, int32 before.
    pub fn parse_from(parser: &mut crate::store_parse::Parser) -> Result<ChannelId, &'static str> {
        if parser.version() >= crate::store_parse::version::SUPPORT_64_BIT_IDS {
            Ok(ChannelId(parser.i64_()?))
        } else {
            Ok(ChannelId(parser.i32_()? as i64))
        }
    }
}

/// `td::ChannelType` (ChannelType.h) — the uint8 `Broadcast`/`Megagroup`/
/// `Unknown` classification `Channel::get_channel_type` computes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ChannelType {
    Broadcast,
    Megagroup,
    Unknown,
}

/// `td::PublicDialogType` (PublicDialogType.h). The discriminants are pinned:
/// `static_cast<int32>` indexes the `public_channels<N>` binlog-pmc keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(i32)]
pub enum PublicDialogType {
    HasUsername = 0,
    IsLocationBased = 1,
    ForPersonalDialog = 2,
}

/// `td::MessageType` (MessageId.h:22).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MessageType {
    #[default]
    None,
    Server,
    YetUnsent,
    Local,
}

/// `td::MessageId` (MessageId.h) — the full bit layout:
///
/// ```text
/// ordinary:  |server_message_id:31|local_id:17|1|type:2|
/// scheduled: |send_date-2^30:30|server_id:18|1|type:2|
/// ```
///
/// The derived `Ord` is the C++ `operator<` over the raw id (whose
/// same-scheduledness `CHECK` is diagnostic only); the peer/message
/// constructors (`get_message_id`, `get_max_message_id`,
/// `is_message_id_order_*`, MessageId.cpp:27-102) land with the message
/// intake (MessagesManager step).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MessageId(i64);

impl MessageId {
    /// `SERVER_ID_SHIFT` (MessageId.h:27).
    const SERVER_ID_SHIFT: i32 = 20;
    /// `SHORT_TYPE_MASK` (MessageId.h:28).
    const SHORT_TYPE_MASK: i64 = (1 << 2) - 1;
    /// `TYPE_MASK` (MessageId.h:29).
    const TYPE_MASK: i64 = (1 << 3) - 1;
    /// `FULL_TYPE_MASK` (MessageId.h:30).
    const FULL_TYPE_MASK: i64 = (1 << MessageId::SERVER_ID_SHIFT) - 1;
    /// `SCHEDULED_MASK` (MessageId.h:31).
    const SCHEDULED_MASK: i64 = 4;
    /// `TYPE_YET_UNSENT` (MessageId.h:32).
    const TYPE_YET_UNSENT: i64 = 1;
    /// `TYPE_LOCAL` (MessageId.h:33).
    const TYPE_LOCAL: i64 = 2;

    pub fn new(id: i64) -> MessageId {
        MessageId(id)
    }

    /// `MessageId(ServerMessageId)` (MessageId.h:59-61).
    pub fn from_server_message_id(
        server_message_id: crate::dialog_id::ServerMessageId,
    ) -> MessageId {
        MessageId((server_message_id.get() as i64) << Self::SERVER_ID_SHIFT)
    }

    /// `MessageId(ScheduledServerMessageId, send_date, force)`
    /// (MessageId.cpp:13-25) — both validation failures reset to 0 with the
    /// C++ `LOG(ERROR)`.
    pub fn from_scheduled(
        server_message_id: crate::dialog_id::ScheduledServerMessageId,
        send_date: i32,
        force: bool,
    ) -> MessageId {
        if send_date <= (1 << 30) {
            tracing::error!("Scheduled message send date {send_date} is in the past");
            return MessageId::new(0);
        }
        if !server_message_id.is_valid() && !force {
            tracing::error!(
                "Scheduled message ID {} is invalid",
                server_message_id.get()
            );
            return MessageId::new(0);
        }
        MessageId::new(
            ((send_date - (1 << 30)) as i64) << 21
                | (server_message_id.get() as i64) << 3
                | Self::SCHEDULED_MASK,
        )
    }

    /// `MessageId::min` (MessageId.h:70-72).
    pub const MIN: MessageId = MessageId(Self::TYPE_YET_UNSENT);

    /// `MessageId::max` (MessageId.h:73-75).
    pub const MAX: MessageId = MessageId((i32::MAX as i64) << Self::SERVER_ID_SHIFT);

    /// `MessageId::min` (MessageId.h:70-72).
    #[allow(clippy::should_implement_trait)]
    pub fn min() -> MessageId {
        Self::MIN
    }

    /// `MessageId::max` (MessageId.h:73-75).
    #[allow(clippy::should_implement_trait)]
    pub fn max() -> MessageId {
        Self::MAX
    }

    pub fn get(self) -> i64 {
        self.0
    }

    /// `MessageId::get_message_id(message, is_scheduled)`
    /// (MessageId.cpp:28-47) — the wire constructor; the Rust name
    /// retires the C++ overload set.
    pub fn from_telegram_message(
        message: &tdmtproto::telegram_api::Message,
        is_scheduled: bool,
    ) -> MessageId {
        match message {
            tdmtproto::telegram_api::Message::messageEmpty(message) => {
                if is_scheduled {
                    MessageId::new(0)
                } else {
                    MessageId::from_server_message_id(crate::dialog_id::ServerMessageId::new(
                        message.id,
                    ))
                }
            }
            tdmtproto::telegram_api::Message::message(message) => {
                if is_scheduled {
                    MessageId::from_scheduled(
                        crate::dialog_id::ScheduledServerMessageId::new(message.id),
                        message.date,
                        false,
                    )
                } else {
                    MessageId::from_server_message_id(crate::dialog_id::ServerMessageId::new(
                        message.id,
                    ))
                }
            }
            tdmtproto::telegram_api::Message::messageService(message) => {
                if is_scheduled {
                    MessageId::from_scheduled(
                        crate::dialog_id::ScheduledServerMessageId::new(message.id),
                        message.date,
                        false,
                    )
                } else {
                    MessageId::from_server_message_id(crate::dialog_id::ServerMessageId::new(
                        message.id,
                    ))
                }
            }
        }
    }

    /// `MessageId::is_valid` (MessageId.cpp:104-117).
    pub fn is_valid(self) -> bool {
        if self.0 <= 0 || self.0 > Self::max().get() {
            return false;
        }
        if (self.0 & Self::FULL_TYPE_MASK) == 0 {
            return true;
        }
        let type_ = self.0 & Self::TYPE_MASK;
        type_ == Self::TYPE_YET_UNSENT || type_ == Self::TYPE_LOCAL
    }

    /// `MessageId::is_valid_scheduled` (MessageId.cpp:119-127).
    pub fn is_valid_scheduled(self) -> bool {
        if self.0 <= 0 || self.0 > (1i64 << 51) {
            return false;
        }
        let type_ = self.0 & Self::TYPE_MASK;
        type_ == Self::SCHEDULED_MASK
            || type_ == (Self::SCHEDULED_MASK | Self::TYPE_YET_UNSENT)
            || type_ == (Self::SCHEDULED_MASK | Self::TYPE_LOCAL)
    }

    /// `MessageId::is_valid_sponsored` (MessageId.cpp:129-137).
    pub fn is_valid_sponsored(self) -> bool {
        if self.0 <= Self::max().get() || self.0 > (1i64 << 51) {
            return false;
        }
        self.0 & Self::TYPE_MASK == Self::TYPE_LOCAL
    }

    /// `MessageId::get_type` (MessageId.cpp:139-168). The type words are
    /// matched with guards — a pattern `a | b` would be an or-pattern, not
    /// the C++ bitwise disjunction.
    pub fn get_type(self) -> MessageType {
        if self.0 <= 0 || self.0 > (1i64 << 51) {
            return MessageType::None;
        }
        if self.is_scheduled() {
            let type_ = self.0 & Self::TYPE_MASK;
            if type_ == Self::SCHEDULED_MASK | Self::TYPE_YET_UNSENT {
                return MessageType::YetUnsent;
            }
            if type_ == Self::SCHEDULED_MASK | Self::TYPE_LOCAL {
                return MessageType::Local;
            }
            if type_ == Self::SCHEDULED_MASK {
                return MessageType::Server;
            }
            return MessageType::None;
        }
        if (self.0 & Self::FULL_TYPE_MASK) == 0 {
            return MessageType::Server;
        }
        let type_ = self.0 & Self::TYPE_MASK;
        if type_ == Self::TYPE_YET_UNSENT {
            return MessageType::YetUnsent;
        }
        if type_ == Self::TYPE_LOCAL {
            return MessageType::Local;
        }
        MessageType::None
    }

    /// `MessageId::is_scheduled` (MessageId.h:107-109).
    pub fn is_scheduled(self) -> bool {
        (self.0 & Self::SCHEDULED_MASK) != 0
    }

    /// `MessageId::is_yet_unsent` (MessageId.h:111-114).
    pub fn is_yet_unsent(self) -> bool {
        assert!(self.is_valid() || self.is_scheduled());
        (self.0 & Self::SHORT_TYPE_MASK) == Self::TYPE_YET_UNSENT
    }

    /// `MessageId::is_local` (MessageId.h:116-119).
    pub fn is_local(self) -> bool {
        assert!(self.is_valid() || self.is_scheduled());
        (self.0 & Self::SHORT_TYPE_MASK) == Self::TYPE_LOCAL
    }

    /// `MessageId::is_server` (MessageId.h:121-124) — also checks validness.
    pub fn is_server(self) -> bool {
        (self.0 & Self::FULL_TYPE_MASK) == 0 && self.0 > 0 && self.0 <= Self::max().get()
    }

    /// `MessageId::is_scheduled_server` (MessageId.h:126-129).
    pub fn is_scheduled_server(self) -> bool {
        assert!(self.is_valid_scheduled());
        (self.0 & Self::SHORT_TYPE_MASK) == 0
    }

    /// `MessageId::is_any_server` (MessageId.h:131-133).
    pub fn is_any_server(self) -> bool {
        if self.is_scheduled() {
            self.is_scheduled_server()
        } else {
            self.is_server()
        }
    }

    /// `MessageId::get_server_message_id_force` (MessageId.cpp:170-174).
    fn get_server_message_id_force(self) -> crate::dialog_id::ServerMessageId {
        assert!(!self.is_scheduled());
        crate::dialog_id::ServerMessageId::new((self.0 >> Self::SERVER_ID_SHIFT) as i32)
    }

    /// `MessageId::get_server_message_id` (MessageId.h:135-138).
    pub fn get_server_message_id(self) -> crate::dialog_id::ServerMessageId {
        assert!(self.0 == 0 || self.is_server());
        self.get_server_message_id_force()
    }

    /// `MessageId::get_prev_server_message_id` (MessageId.h:141-144) — the
    /// greatest server message identifier not bigger than this one.
    pub fn get_prev_server_message_id(self) -> MessageId {
        assert!(!self.is_scheduled());
        MessageId(self.0 & !Self::FULL_TYPE_MASK)
    }

    /// `MessageId::get_next_server_message_id` (MessageId.h:146-150) — the
    /// smallest server message identifier not less than this one.
    pub fn get_next_server_message_id(self) -> MessageId {
        assert!(!self.is_scheduled());
        MessageId((self.0 + Self::FULL_TYPE_MASK) & !Self::FULL_TYPE_MASK)
    }

    /// `MessageId::get_scheduled_server_message_id_force` (MessageId.h:51-54).
    fn get_scheduled_server_message_id_force(self) -> crate::dialog_id::ScheduledServerMessageId {
        assert!(self.is_scheduled());
        crate::dialog_id::ScheduledServerMessageId::new(((self.0 >> 3) & ((1 << 18) - 1)) as i32)
    }

    /// `MessageId::get_scheduled_server_message_id` (MessageId.h:154-157).
    pub fn get_scheduled_server_message_id(self) -> crate::dialog_id::ScheduledServerMessageId {
        assert!(self.is_scheduled_server());
        self.get_scheduled_server_message_id_force()
    }

    /// `MessageId::get_scheduled_message_date` (MessageId.h:159-162).
    pub fn get_scheduled_message_date(self) -> i32 {
        assert!(self.is_valid_scheduled());
        (self.0 >> 21) as i32 + (1 << 30)
    }

    /// `MessageId::get_next_message_id` (MessageId.cpp:176-205).
    pub fn get_next_message_id(self, type_: MessageType) -> MessageId {
        if self.is_scheduled() {
            assert!(self.is_valid_scheduled());
            let current_type = self.get_type();
            if (current_type as i32) < (type_ as i32) {
                return MessageId::new(self.0 - (current_type as i64) + (type_ as i64));
            }
            let base_id = (self.0 & !Self::TYPE_MASK) + Self::TYPE_MASK + 1 + Self::SCHEDULED_MASK;
            return match type_ {
                MessageType::Server => MessageId::new(base_id),
                MessageType::YetUnsent => MessageId::new(base_id + Self::TYPE_YET_UNSENT),
                MessageType::Local => MessageId::new(base_id + Self::TYPE_LOCAL),
                MessageType::None => unreachable!(),
            };
        }
        match type_ {
            MessageType::Server => {
                if self.is_server() {
                    MessageId::from_server_message_id(crate::dialog_id::ServerMessageId::new(
                        self.get_server_message_id().get() + 1,
                    ))
                } else {
                    self.get_next_server_message_id()
                }
            }
            MessageType::YetUnsent => MessageId::new(
                ((self.0 + Self::TYPE_MASK + 1 - Self::TYPE_YET_UNSENT) & !Self::TYPE_MASK)
                    + Self::TYPE_YET_UNSENT,
            ),
            MessageType::Local => MessageId::new(
                ((self.0 + Self::TYPE_MASK + 1 - Self::TYPE_LOCAL) & !Self::TYPE_MASK)
                    + Self::TYPE_LOCAL,
            ),
            MessageType::None => unreachable!(),
        }
    }

    /// `MessageId::store` (MessageId.h:192-195).
    pub fn store(self, out: &mut Vec<u8>) {
        crate::store_parse::store_i64(out, self.0);
    }

    /// `MessageId::parse` (MessageId.h:197-200).
    pub fn parse_from(parser: &mut crate::store_parse::Parser) -> Result<MessageId, &'static str> {
        Ok(MessageId::new(parser.i64_()?))
    }
}

/// `td::DcId` (telegram/net/DcId.h) — the raw id plus the external marker; the
/// non-positive sentinels are `Empty`(0)/`MainDc`(-1)/`Invalid`(-2). The
/// C++'s static `is_valid(int32)` is `is_valid_raw` here (the instance
/// `is_valid` stays private in the C++ and maps to [`DcId::is_exact`] or
/// [`DcId::is_main`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DcId {
    dc_id: i32,
    is_external: bool,
}

impl DcId {
    pub const MAX_RAW_DC_ID: i32 = 1000;

    const EMPTY: i32 = 0;
    const MAIN_DC: i32 = -1;
    const INVALID: i32 = -2;

    fn new(dc_id: i32, is_external: bool) -> DcId {
        DcId { dc_id, is_external }
    }

    /// `DcId::is_valid` (static): the raw-id range gate.
    pub fn is_valid_raw(dc_id: i32) -> bool {
        (1..=Self::MAX_RAW_DC_ID).contains(&dc_id)
    }

    pub fn main() -> DcId {
        DcId::new(Self::MAIN_DC, false)
    }

    pub fn invalid() -> DcId {
        DcId::new(Self::INVALID, false)
    }

    pub fn internal(id: i32) -> DcId {
        assert!(Self::is_valid_raw(id));
        DcId::new(id, false)
    }

    pub fn external(id: i32) -> DcId {
        assert!(Self::is_valid_raw(id));
        DcId::new(id, true)
    }

    pub fn empty() -> DcId {
        DcId::new(Self::EMPTY, false)
    }

    pub fn from_value(value: i32) -> DcId {
        DcId::new(value, false)
    }

    pub fn create(dc_id_value: i32) -> DcId {
        if Self::is_valid_raw(dc_id_value) {
            DcId::new(dc_id_value, false)
        } else {
            DcId::invalid()
        }
    }

    pub fn is_empty(self) -> bool {
        !self.is_exact() && !self.is_main()
    }

    pub fn is_main(self) -> bool {
        self.dc_id == Self::MAIN_DC
    }

    /// `get_raw_id` — the C++ CHECKs `is_exact`, so do we.
    pub fn get_raw_id(self) -> i32 {
        assert!(self.is_exact());
        self.dc_id
    }

    pub fn get_value(self) -> i32 {
        self.dc_id
    }

    pub fn is_internal(self) -> bool {
        !self.is_external()
    }

    pub fn is_external(self) -> bool {
        self.is_external
    }

    pub fn is_exact(self) -> bool {
        self.dc_id > 0
    }
}

/// `DcId::operator<<` (net/DcId.h:98-114).
impl std::fmt::Display for DcId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "DcId{{")?;
        if *self == DcId::invalid() {
            write!(f, "invalid")?;
        } else if *self == DcId::default() {
            write!(f, "empty")?;
        } else if self.is_empty() {
            write!(f, "is_empty")?;
        } else if self.is_main() {
            write!(f, "main")?;
        } else {
            write!(f, "{}", self.get_raw_id())?;
            if self.is_external() {
                write!(f, " external")?;
            }
        }
        write!(f, "}}")
    }
}

/// `td::FileSourceId` (files/FileSourceId.h).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileSourceId(i32);

impl FileSourceId {
    pub fn new(id: i32) -> FileSourceId {
        FileSourceId(id)
    }

    pub fn is_valid(self) -> bool {
        self.0 > 0
    }

    pub fn get(self) -> i32 {
        self.0
    }
}

/// `td::StickerSetId` (StickerSetId.h) — any non-zero id is valid.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StickerSetId(i64);

impl StickerSetId {
    pub fn new(id: i64) -> StickerSetId {
        StickerSetId(id)
    }

    pub fn is_valid(self) -> bool {
        self.0 != 0
    }

    pub fn get(self) -> i64 {
        self.0
    }
}

/// `td::DialogType` (DialogId.h:23).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DialogType {
    None,
    User,
    Chat,
    Channel,
    SecretChat,
}

/// `td::DialogId` (DialogId.h:25): the signed polymorphic dialog handle. The
/// negative ranges stack downward from the basic-group band (`-ChatId`,
/// `ZERO_CHANNEL_ID - ChannelId`, `ZERO_SECRET_CHAT_ID + SecretChatId` and the
/// monoforum tail); users stay positive. The four continuity pins of
/// `DialogId::get_type` are compile-time asserts here.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DialogId(i64);

// DialogId.cpp:22-26 — the valid ranges are continuous.
const _: () = assert!(DialogId::ZERO_CHANNEL_ID + 1 == -ChatId::MAX_CHAT_ID);
const _: () = assert!(
    DialogId::ZERO_SECRET_CHAT_ID + i32::MAX as i64 + 1
        == DialogId::ZERO_CHANNEL_ID - ChannelId::MAX_CHANNEL_ID
);
const _: () = assert!(
    DialogId::ZERO_CHANNEL_ID - ChannelId::MIN_MONOFORUM_CHANNEL_ID + 1
        == DialogId::ZERO_SECRET_CHAT_ID + i32::MIN as i64
);

impl DialogId {
    const ZERO_SECRET_CHAT_ID: i64 = -2_000_000_000_000;
    const ZERO_CHANNEL_ID: i64 = -1_000_000_000_000;

    /// The C++ `explicit constexpr DialogId(int64)` — no validity check.
    pub const fn new(dialog_id: i64) -> DialogId {
        DialogId(dialog_id)
    }

    /// `DialogId::DialogId(UserId)` — an invalid user collapses to 0.
    pub fn from_user_id(user_id: UserId) -> DialogId {
        if user_id.is_valid() {
            DialogId(user_id.get())
        } else {
            DialogId(0)
        }
    }

    /// `DialogId::DialogId(ChatId)`.
    pub fn from_chat_id(chat_id: ChatId) -> DialogId {
        if chat_id.is_valid() {
            DialogId(-chat_id.get())
        } else {
            DialogId(0)
        }
    }

    /// `DialogId::DialogId(ChannelId)`.
    pub fn from_channel_id(channel_id: ChannelId) -> DialogId {
        if channel_id.is_valid() {
            DialogId(Self::ZERO_CHANNEL_ID - channel_id.get())
        } else {
            DialogId(0)
        }
    }

    /// `DialogId::DialogId(SecretChatId)`.
    pub fn from_secret_chat_id(secret_chat_id: SecretChatId) -> DialogId {
        if secret_chat_id.is_valid() {
            DialogId(Self::ZERO_SECRET_CHAT_ID + secret_chat_id.get() as i64)
        } else {
            DialogId(0)
        }
    }

    /// `DialogId::DialogId(const telegram_api::Peer &)`.
    pub fn from_peer(peer: &telegram_api::Peer) -> DialogId {
        DialogId(Self::get_peer_id(peer))
    }

    /// `DialogId::DialogId(const tl_object_ptr<telegram_api::DialogPeer> &)`.
    pub fn from_dialog_peer(dialog_peer: &telegram_api::DialogPeer) -> DialogId {
        match dialog_peer {
            telegram_api::DialogPeer::dialogPeer(peer) => {
                DialogId(Self::get_peer_id(peer.peer.as_ref().unwrap()))
            }
            telegram_api::DialogPeer::dialogPeerFolder(_) => {
                tdutils::log_error!("Receive unsupported dialog peer {:?}", dialog_peer);
                DialogId(0)
            }
            telegram_api::DialogPeer::dialogPeerCommunity(_) => {
                tdutils::log_error!("Receive unsupported dialog peer {:?}", dialog_peer);
                DialogId(0)
            }
        }
    }

    /// `DialogId::get_peer_id` — an invalid peer id logs and collapses to 0.
    fn get_peer_id(peer: &telegram_api::Peer) -> i64 {
        match peer {
            telegram_api::Peer::peerUser(peer_user) => {
                let user_id = UserId::new(peer_user.user_id);
                if !user_id.is_valid() {
                    tdutils::log_error!("Receive invalid user id {}", user_id.get());
                    return 0;
                }
                user_id.get()
            }
            telegram_api::Peer::peerChat(peer_chat) => {
                let chat_id = ChatId::new(peer_chat.chat_id);
                if !chat_id.is_valid() {
                    tdutils::log_error!("Receive invalid chat id {}", chat_id.get());
                    return 0;
                }
                -chat_id.get()
            }
            telegram_api::Peer::peerChannel(peer_channel) => {
                let channel_id = ChannelId::new(peer_channel.channel_id);
                if !channel_id.is_valid() {
                    tdutils::log_error!("Receive invalid channel id {}", channel_id.get());
                    return 0;
                }
                Self::ZERO_CHANNEL_ID - channel_id.get()
            }
        }
    }

    pub fn get(self) -> i64 {
        self.0
    }

    /// `DialogId::is_valid` — `get_type() != DialogType::None`.
    pub fn is_valid(self) -> bool {
        self.get_type() != DialogType::None
    }

    /// `DialogId::get_type` (DialogId.cpp:20-44) — the branch order is the
    /// range table; the two `!= 0` guards carve `ZERO_CHANNEL_ID` and
    /// `ZERO_SECRET_CHAT_ID` out of their otherwise-matching windows.
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

    /// `DialogId::get_user_id` — the C++ CHECKs the type.
    pub fn get_user_id(self) -> UserId {
        assert!(
            self.get_type() == DialogType::User,
            "DialogId is not a user"
        );
        UserId::new(self.0)
    }

    /// `DialogId::get_chat_id`.
    pub fn get_chat_id(self) -> ChatId {
        assert!(
            self.get_type() == DialogType::Chat,
            "DialogId is not a chat"
        );
        ChatId::new(-self.0)
    }

    /// `DialogId::get_channel_id`.
    pub fn get_channel_id(self) -> ChannelId {
        assert!(
            self.get_type() == DialogType::Channel,
            "DialogId is not a channel"
        );
        ChannelId::new(Self::ZERO_CHANNEL_ID - self.0)
    }

    /// `DialogId::get_secret_chat_id` — the C++ truncates back to `int32`.
    pub fn get_secret_chat_id(self) -> SecretChatId {
        assert!(
            self.get_type() == DialogType::SecretChat,
            "DialogId is not a secret chat"
        );
        SecretChatId::new((self.0 - Self::ZERO_SECRET_CHAT_ID) as i32)
    }

    /// `DialogId::store` — the raw id, `store_long`.
    pub fn store(&self, out: &mut Vec<u8>) {
        store_i64(out, self.0);
    }

    /// `DialogId::parse` — `fetch_long`.
    pub fn parse_impl(parser: &mut Parser<'_>) -> Result<DialogId, &'static str> {
        Ok(DialogId(parser.i64_()?))
    }
}

/// `td::AdministratorRights` (DialogParticipant.h:24): the permission set of a
/// chat administrator. The flags are the server-side bit layout — the classic
/// admin bits in the low range, `IS_ANONYMOUS` at bit 13 and the story/
/// community-era bits at 48..54 — and `ALL_ADMINISTRATOR_RIGHTS` is the
/// server-parity mask every constructor result is gated through.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AdministratorRights {
    flags: u64,
}

impl AdministratorRights {
    const CAN_CHANGE_INFO_AND_SETTINGS: u64 = 1 << 0;
    const CAN_POST_MESSAGES: u64 = 1 << 1;
    const CAN_EDIT_MESSAGES: u64 = 1 << 2;
    const CAN_DELETE_MESSAGES: u64 = 1 << 3;
    const CAN_INVITE_USERS: u64 = 1 << 4;
    // const CAN_EXPORT_DIALOG_INVITE_LINK: u64 = 1 << 5; // no longer exists
    const CAN_RESTRICT_MEMBERS: u64 = 1 << 6;
    const CAN_PIN_MESSAGES: u64 = 1 << 7;
    const CAN_PROMOTE_MEMBERS: u64 = 1 << 8;
    const CAN_MANAGE_CALLS: u64 = 1 << 9;
    const CAN_MANAGE_DIALOG: u64 = 1 << 10;
    const CAN_MANAGE_TOPICS: u64 = 1 << 11;
    // Kept for C++ surface parity — unused in the C++ too.
    #[allow(dead_code)]
    const LEGACY_CAN_SEND_MEDIA: u64 = 1 << 17;
    const CAN_POST_STORIES: u64 = 1 << 48;
    const CAN_EDIT_STORIES: u64 = 1 << 49;
    const CAN_DELETE_STORIES: u64 = 1 << 50;
    const CAN_MANAGE_DIRECT_MESSAGES: u64 = 1 << 51;
    const CAN_MANAGE_RANKS: u64 = 1 << 52;
    const CAN_MANAGE_LINKED_PEERS: u64 = 1 << 53;
    const CAN_MANAGE_WELCOME_MESSAGES: u64 = 1 << 54;
    pub(crate) const IS_ANONYMOUS: u64 = 1 << 13;

    pub(crate) const ALL_ADMINISTRATOR_RIGHTS: u64 = Self::CAN_CHANGE_INFO_AND_SETTINGS
        | Self::CAN_POST_MESSAGES
        | Self::CAN_EDIT_MESSAGES
        | Self::CAN_DELETE_MESSAGES
        | Self::CAN_INVITE_USERS
        | Self::CAN_RESTRICT_MEMBERS
        | Self::CAN_PIN_MESSAGES
        | Self::CAN_MANAGE_TOPICS
        | Self::CAN_PROMOTE_MEMBERS
        | Self::CAN_MANAGE_CALLS
        | Self::CAN_MANAGE_DIALOG
        | Self::CAN_POST_STORIES
        | Self::CAN_EDIT_STORIES
        | Self::CAN_DELETE_STORIES
        | Self::CAN_MANAGE_DIRECT_MESSAGES
        | Self::CAN_MANAGE_RANKS
        | Self::CAN_MANAGE_LINKED_PEERS
        | Self::CAN_MANAGE_WELCOME_MESSAGES;

    /// The C++ `friend class DialogParticipantStatus` constructor: the raw
    /// flags masked to the server-known set (DialogParticipantStatus embeds
    /// the rights in its own flag word, M.5.3).
    pub(crate) fn from_flags(flags: u64) -> AdministratorRights {
        AdministratorRights {
            flags: flags & (Self::ALL_ADMINISTRATOR_RIGHTS | Self::IS_ANONYMOUS),
        }
    }

    /// The raw flag word (the C++ friend access from `DialogParticipantStatus`).
    pub(crate) const fn flags(&self) -> u64 {
        self.flags
    }

    /// `AdministratorRights(const telegram_api::chatAdminRights &, ChannelType)`.
    pub fn from_chat_admin_rights(
        rights: Option<&telegram_api::ChatAdminRights>,
        channel_type: ChannelType,
    ) -> AdministratorRights {
        let Some(rights) = rights else {
            return AdministratorRights::default();
        };
        let mut other = rights.other;
        if !other {
            if channel_type == ChannelType::Unknown {
                other = true;
            } else {
                tdutils::log_error!("Receive wrong other flag in {:?}", rights);
            }
        }
        AdministratorRights::from_bools(
            rights.anonymous,
            other,
            rights.change_info,
            rights.post_messages,
            rights.edit_messages,
            rights.delete_messages,
            rights.invite_users,
            rights.ban_users,
            rights.pin_messages,
            rights.manage_topics,
            rights.add_admins,
            rights.manage_call,
            rights.post_stories,
            rights.edit_stories,
            rights.delete_stories,
            rights.manage_direct_messages,
            rights.manage_ranks,
            rights.manage_linked_peers,
            rights.manage_welcome_messages,
            channel_type,
        )
    }

    /// `AdministratorRights(const td_api::chatAdministratorRights &, ChannelType)`.
    pub fn from_chat_administrator_rights(
        rights: Option<&td_api::ChatAdministratorRights>,
        channel_type: ChannelType,
    ) -> AdministratorRights {
        let Some(rights) = rights else {
            return AdministratorRights::default();
        };
        AdministratorRights::from_bools(
            rights.is_anonymous,
            rights.can_manage_chat,
            rights.can_change_info,
            rights.can_post_messages,
            rights.can_edit_messages,
            rights.can_delete_messages,
            rights.can_invite_users,
            rights.can_restrict_members,
            rights.can_pin_messages,
            rights.can_manage_topics,
            rights.can_promote_members,
            rights.can_manage_video_chats,
            rights.can_post_stories,
            rights.can_edit_stories,
            rights.can_delete_stories,
            rights.can_manage_direct_messages,
            rights.can_manage_tags,
            false,
            rights.can_send_welcome_messages,
            channel_type,
        )
    }

    /// `AdministratorRights(const td_api::communityAdministratorRights &)`.
    pub fn from_community_administrator_rights(
        rights: Option<&td_api::CommunityAdministratorRights>,
    ) -> AdministratorRights {
        let Some(rights) = rights else {
            return AdministratorRights::default();
        };
        AdministratorRights::from_bools(
            false,
            rights.can_manage_community,
            rights.can_change_info,
            false,
            false,
            false,
            false,
            rights.can_ban_members,
            false,
            false,
            rights.can_promote_members,
            false,
            false,
            false,
            false,
            false,
            false,
            rights.can_edit_chat_list,
            false,
            ChannelType::Unknown,
        )
    }

    /// `AdministratorRights(bool is_anonymous, bool can_manage_dialog, ..., ChannelType)`:
    /// the common constructor. The channel type zeroes the flags that make no
    /// sense there, and any non-empty set is upgraded with `CAN_MANAGE_DIALOG`.
    #[allow(clippy::too_many_arguments)] // the C++ constructor has the same shape
    pub fn from_bools(
        mut is_anonymous: bool,
        can_manage_dialog: bool,
        can_change_info: bool,
        mut can_post_messages: bool,
        mut can_edit_messages: bool,
        can_delete_messages: bool,
        can_invite_users: bool,
        can_restrict_members: bool,
        mut can_pin_messages: bool,
        mut can_manage_topics: bool,
        can_promote_members: bool,
        can_manage_calls: bool,
        can_post_stories: bool,
        can_edit_stories: bool,
        can_delete_stories: bool,
        mut can_manage_direct_messages: bool,
        mut can_manage_ranks: bool,
        can_manage_linked_peers: bool,
        can_manage_welcome_messages: bool,
        channel_type: ChannelType,
    ) -> AdministratorRights {
        match channel_type {
            ChannelType::Broadcast => {
                can_pin_messages = false;
                can_manage_topics = false;
                can_manage_ranks = false;
                is_anonymous = false;
            }
            ChannelType::Megagroup => {
                can_post_messages = false;
                can_edit_messages = false;
                can_manage_direct_messages = false;
            }
            ChannelType::Unknown => {
                can_manage_topics = false;
            }
        }
        let mut flags = (u64::from(can_manage_dialog) * Self::CAN_MANAGE_DIALOG)
            | (u64::from(can_change_info) * Self::CAN_CHANGE_INFO_AND_SETTINGS)
            | (u64::from(can_post_messages) * Self::CAN_POST_MESSAGES)
            | (u64::from(can_edit_messages) * Self::CAN_EDIT_MESSAGES)
            | (u64::from(can_delete_messages) * Self::CAN_DELETE_MESSAGES)
            | (u64::from(can_invite_users) * Self::CAN_INVITE_USERS)
            | (u64::from(can_restrict_members) * Self::CAN_RESTRICT_MEMBERS)
            | (u64::from(can_pin_messages) * Self::CAN_PIN_MESSAGES)
            | (u64::from(can_manage_topics) * Self::CAN_MANAGE_TOPICS)
            | (u64::from(can_promote_members) * Self::CAN_PROMOTE_MEMBERS)
            | (u64::from(can_manage_calls) * Self::CAN_MANAGE_CALLS)
            | (u64::from(can_post_stories) * Self::CAN_POST_STORIES)
            | (u64::from(can_edit_stories) * Self::CAN_EDIT_STORIES)
            | (u64::from(can_delete_stories) * Self::CAN_DELETE_STORIES)
            | (u64::from(can_manage_direct_messages) * Self::CAN_MANAGE_DIRECT_MESSAGES)
            | (u64::from(can_manage_ranks) * Self::CAN_MANAGE_RANKS)
            | (u64::from(can_manage_linked_peers) * Self::CAN_MANAGE_LINKED_PEERS)
            | (u64::from(can_manage_welcome_messages) * Self::CAN_MANAGE_WELCOME_MESSAGES)
            | (u64::from(is_anonymous) * Self::IS_ANONYMOUS);
        if flags != 0 {
            flags |= Self::CAN_MANAGE_DIALOG;
        }
        AdministratorRights { flags }
    }

    /// `AdministratorRights::get_chat_admin_rights`.
    pub fn get_chat_admin_rights(&self) -> telegram_api::ChatAdminRights {
        telegram_api::ChatAdminRights {
            flags: 0,
            change_info: self.can_change_info_and_settings(),
            post_messages: self.can_post_messages(),
            edit_messages: self.can_edit_messages(),
            delete_messages: self.can_delete_messages(),
            ban_users: self.can_restrict_members(),
            invite_users: self.can_invite_users(),
            pin_messages: self.can_pin_messages(),
            add_admins: self.can_promote_members(),
            anonymous: self.is_anonymous(),
            manage_call: self.can_manage_calls(),
            other: self.can_manage_dialog(),
            manage_topics: self.can_manage_topics(),
            post_stories: self.can_post_stories(),
            edit_stories: self.can_edit_stories(),
            delete_stories: self.can_delete_stories(),
            manage_direct_messages: self.can_manage_direct_messages(),
            manage_ranks: self.can_manage_ranks(),
            manage_linked_peers: self.can_manage_linked_peers(),
            manage_welcome_messages: self.can_manage_welcome_messages(),
        }
    }

    /// `AdministratorRights::get_chat_administrator_rights_object`.
    pub fn get_chat_administrator_rights_object(&self) -> td_api::ChatAdministratorRights {
        td_api::ChatAdministratorRights {
            can_manage_chat: self.can_manage_dialog(),
            can_change_info: self.can_change_info_and_settings(),
            can_post_messages: self.can_post_messages(),
            can_edit_messages: self.can_edit_messages(),
            can_delete_messages: self.can_delete_messages(),
            can_invite_users: self.can_invite_users(),
            can_restrict_members: self.can_restrict_members(),
            can_pin_messages: self.can_pin_messages(),
            can_manage_topics: self.can_manage_topics(),
            can_promote_members: self.can_promote_members(),
            can_manage_video_chats: self.can_manage_calls(),
            can_post_stories: self.can_post_stories(),
            can_edit_stories: self.can_edit_stories(),
            can_delete_stories: self.can_delete_stories(),
            can_manage_direct_messages: self.can_manage_direct_messages(),
            can_manage_tags: self.can_manage_ranks(),
            can_send_welcome_messages: self.can_manage_welcome_messages(),
            is_anonymous: self.is_anonymous(),
        }
    }

    /// `AdministratorRights::get_community_administrator_rights_object`.
    pub fn get_community_administrator_rights_object(
        &self,
    ) -> td_api::CommunityAdministratorRights {
        td_api::CommunityAdministratorRights {
            can_manage_community: self.can_manage_dialog(),
            can_change_info: self.can_change_info_and_settings(),
            can_edit_chat_list: self.can_manage_linked_peers(),
            can_promote_members: self.can_promote_members(),
            can_ban_members: self.can_restrict_members(),
        }
    }

    pub fn can_manage_dialog(&self) -> bool {
        self.flags & Self::CAN_MANAGE_DIALOG != 0
    }

    pub fn can_change_info_and_settings(&self) -> bool {
        self.flags & Self::CAN_CHANGE_INFO_AND_SETTINGS != 0
    }

    pub fn can_post_messages(&self) -> bool {
        self.flags & Self::CAN_POST_MESSAGES != 0
    }

    pub fn can_edit_messages(&self) -> bool {
        self.flags & Self::CAN_EDIT_MESSAGES != 0
    }

    pub fn can_delete_messages(&self) -> bool {
        self.flags & Self::CAN_DELETE_MESSAGES != 0
    }

    pub fn can_invite_users(&self) -> bool {
        self.flags & Self::CAN_INVITE_USERS != 0
    }

    pub fn can_restrict_members(&self) -> bool {
        self.flags & Self::CAN_RESTRICT_MEMBERS != 0
    }

    pub fn can_pin_messages(&self) -> bool {
        self.flags & Self::CAN_PIN_MESSAGES != 0
    }

    pub fn can_manage_topics(&self) -> bool {
        self.flags & Self::CAN_MANAGE_TOPICS != 0
    }

    pub fn can_promote_members(&self) -> bool {
        self.flags & Self::CAN_PROMOTE_MEMBERS != 0
    }

    pub fn can_manage_calls(&self) -> bool {
        self.flags & Self::CAN_MANAGE_CALLS != 0
    }

    pub fn can_post_stories(&self) -> bool {
        self.flags & Self::CAN_POST_STORIES != 0
    }

    pub fn can_edit_stories(&self) -> bool {
        self.flags & Self::CAN_EDIT_STORIES != 0
    }

    pub fn can_delete_stories(&self) -> bool {
        self.flags & Self::CAN_DELETE_STORIES != 0
    }

    pub fn can_manage_direct_messages(&self) -> bool {
        self.flags & Self::CAN_MANAGE_DIRECT_MESSAGES != 0
    }

    pub fn can_manage_ranks(&self) -> bool {
        self.flags & Self::CAN_MANAGE_RANKS != 0
    }

    pub fn can_manage_linked_peers(&self) -> bool {
        self.flags & Self::CAN_MANAGE_LINKED_PEERS != 0
    }

    pub fn can_manage_welcome_messages(&self) -> bool {
        self.flags & Self::CAN_MANAGE_WELCOME_MESSAGES != 0
    }

    pub fn is_anonymous(&self) -> bool {
        self.flags & Self::IS_ANONYMOUS != 0
    }

    /// `AdministratorRights::store` (DialogParticipant.h) — `td::store(uint64)`.
    pub fn store(&self, out: &mut Vec<u8>) {
        store_u64(out, self.flags);
    }

    /// `AdministratorRights::parse` — the versioned framing: 64-bit flags
    /// since `MakeParticipantFlags64Bit`, the 32-bit legacy word before.
    pub fn parse_impl(parser: &mut Parser<'_>) -> Result<AdministratorRights, &'static str> {
        let flags = if parser.version() >= version::MAKE_PARTICIPANT_FLAGS_64_BIT {
            parser.u64_()?
        } else {
            u64::from(parser.u32_()?)
        };
        Ok(AdministratorRights { flags })
    }
}

/// `td::RestrictedRights` (DialogParticipant.h:188): the permissions of a
/// restricted (banned-but-present) member. The flags store what the member
/// *can* do, so `restrict_all()` is the empty set.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RestrictedRights {
    flags: u64,
}

impl RestrictedRights {
    const CAN_SEND_MESSAGES: u64 = 1 << 16;
    pub(crate) const LEGACY_CAN_SEND_MEDIA: u64 = 1 << 17;
    pub(crate) const CAN_SEND_AUDIOS: u64 = 1 << 32;
    pub(crate) const CAN_SEND_DOCUMENTS: u64 = 1 << 33;
    pub(crate) const CAN_SEND_PHOTOS: u64 = 1 << 34;
    pub(crate) const CAN_SEND_VIDEOS: u64 = 1 << 35;
    pub(crate) const CAN_SEND_VIDEO_NOTES: u64 = 1 << 36;
    pub(crate) const CAN_SEND_VOICE_NOTES: u64 = 1 << 37;
    const CAN_SEND_STICKERS: u64 = 1 << 18;
    const CAN_SEND_ANIMATIONS: u64 = 1 << 19;
    const CAN_SEND_GAMES: u64 = 1 << 20;
    const CAN_USE_INLINE_BOTS: u64 = 1 << 21;
    const CAN_ADD_WEB_PAGE_PREVIEWS: u64 = 1 << 22;
    const CAN_SEND_POLLS: u64 = 1 << 23;
    const CAN_CHANGE_INFO_AND_SETTINGS: u64 = 1 << 24;
    const CAN_INVITE_USERS: u64 = 1 << 25;
    const CAN_PIN_MESSAGES: u64 = 1 << 26;
    const CAN_MANAGE_TOPICS: u64 = 1 << 12;
    const CAN_EDIT_RANK: u64 = 1 << 38;
    const CAN_SEND_REACTIONS: u64 = 1 << 39;
    const CAN_MANAGE_LINKED_PEERS: u64 = 1 << 40;

    pub(crate) const ALL_ADMIN_PERMISSION_RIGHTS: u64 = Self::CAN_CHANGE_INFO_AND_SETTINGS
        | Self::CAN_INVITE_USERS
        | Self::CAN_PIN_MESSAGES
        | Self::CAN_MANAGE_TOPICS
        | Self::CAN_MANAGE_LINKED_PEERS;

    pub(crate) const ALL_RESTRICTED_RIGHTS: u64 = Self::CAN_SEND_MESSAGES
        | Self::CAN_SEND_STICKERS
        | Self::CAN_SEND_ANIMATIONS
        | Self::CAN_SEND_GAMES
        | Self::CAN_USE_INLINE_BOTS
        | Self::CAN_ADD_WEB_PAGE_PREVIEWS
        | Self::CAN_SEND_POLLS
        | Self::ALL_ADMIN_PERMISSION_RIGHTS
        | Self::CAN_SEND_AUDIOS
        | Self::CAN_SEND_DOCUMENTS
        | Self::CAN_SEND_PHOTOS
        | Self::CAN_SEND_VIDEOS
        | Self::CAN_SEND_VIDEO_NOTES
        | Self::CAN_SEND_VOICE_NOTES
        | Self::CAN_EDIT_RANK
        | Self::CAN_SEND_REACTIONS;

    /// The C++ `friend class DialogParticipantStatus` constructor (M.5.3).
    pub(crate) fn from_flags(flags: u64) -> RestrictedRights {
        RestrictedRights {
            flags: flags & Self::ALL_RESTRICTED_RIGHTS,
        }
    }

    /// The raw flag word (the C++ friend access from `DialogParticipantStatus`).
    pub(crate) const fn flags(&self) -> u64 {
        self.flags
    }

    /// `RestrictedRights(const telegram_api::chatBannedRights &, ChannelType)`.
    pub fn from_chat_banned_rights(
        rights: Option<&telegram_api::ChatBannedRights>,
        channel_type: ChannelType,
    ) -> RestrictedRights {
        let Some(rights) = rights else {
            return RestrictedRights::default();
        };
        if channel_type == ChannelType::Broadcast {
            return RestrictedRights::default();
        }
        if rights.view_messages {
            tdutils::log_error!("Can't view messages in banned rights {:?}", rights);
        }
        if rights.until_date != i32::MAX {
            tdutils::log_error!("Have until date {} in restricted rights", rights.until_date);
        }
        RestrictedRights::from_bools(
            !rights.send_plain,
            !rights.send_audios,
            !rights.send_docs,
            !rights.send_photos,
            !rights.send_videos,
            !rights.send_roundvideos,
            !rights.send_voices,
            !rights.send_stickers,
            !rights.send_gifs,
            !rights.send_games,
            !rights.send_inline,
            !rights.embed_links,
            !rights.send_polls,
            !rights.change_info,
            !rights.invite_users,
            !rights.pin_messages,
            !rights.manage_topics,
            !rights.edit_rank,
            !rights.send_reactions,
            !rights.manage_linked_peers,
            channel_type,
        )
    }

    /// `RestrictedRights(const td_api::chatPermissions &, ChannelType)`.
    pub fn from_chat_permissions(
        rights: Option<&td_api::ChatPermissions>,
        channel_type: ChannelType,
    ) -> RestrictedRights {
        let Some(rights) = rights else {
            return RestrictedRights::default();
        };
        if channel_type == ChannelType::Broadcast {
            return RestrictedRights::default();
        }
        RestrictedRights::from_bools(
            rights.can_send_basic_messages,
            rights.can_send_audios,
            rights.can_send_documents,
            rights.can_send_photos,
            rights.can_send_videos,
            rights.can_send_video_notes,
            rights.can_send_voice_notes,
            rights.can_send_other_messages,
            rights.can_send_other_messages,
            rights.can_send_other_messages,
            rights.can_send_other_messages,
            rights.can_add_link_previews,
            rights.can_send_polls,
            rights.can_change_info,
            rights.can_invite_users,
            rights.can_pin_messages,
            rights.can_create_topics,
            rights.can_edit_tag,
            rights.can_react_to_messages,
            true,
            channel_type,
        )
    }

    /// `RestrictedRights(const td_api::communityPermissions &)`.
    pub fn from_community_permissions(
        rights: Option<&td_api::CommunityPermissions>,
    ) -> RestrictedRights {
        let Some(rights) = rights else {
            return RestrictedRights::default();
        };
        RestrictedRights {
            flags: u64::from(rights.can_edit_chat_list) * Self::CAN_MANAGE_LINKED_PEERS,
        }
    }

    /// `RestrictedRights(bool can_send_messages, ..., ChannelType)`: the
    /// common constructor; a broadcast yields the empty set outright.
    #[allow(clippy::too_many_arguments)] // the C++ constructor has the same shape
    pub fn from_bools(
        can_send_messages: bool,
        can_send_audios: bool,
        can_send_documents: bool,
        can_send_photos: bool,
        can_send_videos: bool,
        can_send_video_notes: bool,
        can_send_voice_notes: bool,
        can_send_stickers: bool,
        can_send_animations: bool,
        can_send_games: bool,
        can_use_inline_bots: bool,
        can_add_web_page_previews: bool,
        can_send_polls: bool,
        can_change_info_and_settings: bool,
        can_invite_users: bool,
        can_pin_messages: bool,
        can_manage_topics: bool,
        can_edit_rank: bool,
        can_send_reactions: bool,
        can_manage_linked_peers: bool,
        channel_type: ChannelType,
    ) -> RestrictedRights {
        if channel_type == ChannelType::Broadcast {
            return RestrictedRights::default();
        }
        let flags = (u64::from(can_send_messages) * Self::CAN_SEND_MESSAGES)
            | (u64::from(can_send_audios) * Self::CAN_SEND_AUDIOS)
            | (u64::from(can_send_documents) * Self::CAN_SEND_DOCUMENTS)
            | (u64::from(can_send_photos) * Self::CAN_SEND_PHOTOS)
            | (u64::from(can_send_videos) * Self::CAN_SEND_VIDEOS)
            | (u64::from(can_send_video_notes) * Self::CAN_SEND_VIDEO_NOTES)
            | (u64::from(can_send_voice_notes) * Self::CAN_SEND_VOICE_NOTES)
            | (u64::from(can_send_stickers) * Self::CAN_SEND_STICKERS)
            | (u64::from(can_send_animations) * Self::CAN_SEND_ANIMATIONS)
            | (u64::from(can_send_games) * Self::CAN_SEND_GAMES)
            | (u64::from(can_use_inline_bots) * Self::CAN_USE_INLINE_BOTS)
            | (u64::from(can_add_web_page_previews) * Self::CAN_ADD_WEB_PAGE_PREVIEWS)
            | (u64::from(can_send_polls) * Self::CAN_SEND_POLLS)
            | (u64::from(can_change_info_and_settings) * Self::CAN_CHANGE_INFO_AND_SETTINGS)
            | (u64::from(can_invite_users) * Self::CAN_INVITE_USERS)
            | (u64::from(can_pin_messages) * Self::CAN_PIN_MESSAGES)
            | (u64::from(can_manage_topics) * Self::CAN_MANAGE_TOPICS)
            | (u64::from(can_edit_rank) * Self::CAN_EDIT_RANK)
            | (u64::from(can_send_reactions) * Self::CAN_SEND_REACTIONS)
            | (u64::from(can_manage_linked_peers) * Self::CAN_MANAGE_LINKED_PEERS);
        RestrictedRights { flags }
    }

    /// `RestrictedRights::restrict_all` — the C++ routes it through the
    /// `td_api::chatPermissions` constructor with a null object.
    pub fn restrict_all() -> RestrictedRights {
        RestrictedRights::from_chat_permissions(None, ChannelType::Unknown)
    }

    /// `RestrictedRights::get_chat_permissions_object`.
    pub fn get_chat_permissions_object(&self) -> td_api::ChatPermissions {
        td_api::ChatPermissions {
            can_send_basic_messages: self.can_send_messages(),
            can_send_audios: self.can_send_audios(),
            can_send_documents: self.can_send_documents(),
            can_send_photos: self.can_send_photos(),
            can_send_videos: self.can_send_videos(),
            can_send_video_notes: self.can_send_video_notes(),
            can_send_voice_notes: self.can_send_voice_notes(),
            can_send_polls: self.can_send_polls(),
            can_send_other_messages: self.can_send_stickers()
                || self.can_send_animations()
                || self.can_send_games()
                || self.can_use_inline_bots(),
            can_add_link_previews: self.can_add_web_page_previews(),
            can_react_to_messages: self.can_send_reactions(),
            can_edit_tag: self.can_edit_rank(),
            can_change_info: self.can_change_info_and_settings(),
            can_invite_users: self.can_invite_users(),
            can_pin_messages: self.can_pin_messages(),
            can_create_topics: self.can_manage_topics(),
        }
    }

    /// `RestrictedRights::get_community_permissions_object`.
    pub fn get_community_permissions_object(&self) -> td_api::CommunityPermissions {
        td_api::CommunityPermissions {
            can_edit_chat_list: self.can_manage_linked_peers(),
        }
    }

    /// `RestrictedRights::get_chat_banned_rights`.
    pub fn get_chat_banned_rights(&self) -> telegram_api::ChatBannedRights {
        telegram_api::ChatBannedRights {
            flags: 0,
            view_messages: false,
            send_messages: false,
            send_media: false,
            send_stickers: !self.can_send_stickers(),
            send_gifs: !self.can_send_animations(),
            send_games: !self.can_send_games(),
            send_inline: !self.can_use_inline_bots(),
            embed_links: !self.can_add_web_page_previews(),
            send_polls: !self.can_send_polls(),
            change_info: !self.can_change_info_and_settings(),
            invite_users: !self.can_invite_users(),
            pin_messages: !self.can_pin_messages(),
            manage_topics: !self.can_manage_topics(),
            send_photos: !self.can_send_photos(),
            send_videos: !self.can_send_videos(),
            send_roundvideos: !self.can_send_video_notes(),
            send_audios: !self.can_send_audios(),
            send_voices: !self.can_send_voice_notes(),
            send_docs: !self.can_send_documents(),
            send_plain: !self.can_send_messages(),
            edit_rank: !self.can_edit_rank(),
            send_reactions: !self.can_send_reactions(),
            manage_linked_peers: !self.can_manage_linked_peers(),
            until_date: 0,
        }
    }

    pub fn can_change_info_and_settings(&self) -> bool {
        self.flags & Self::CAN_CHANGE_INFO_AND_SETTINGS != 0
    }

    pub fn can_invite_users(&self) -> bool {
        self.flags & Self::CAN_INVITE_USERS != 0
    }

    pub fn can_pin_messages(&self) -> bool {
        self.flags & Self::CAN_PIN_MESSAGES != 0
    }

    pub fn can_manage_topics(&self) -> bool {
        self.flags & Self::CAN_MANAGE_TOPICS != 0
    }

    pub fn can_send_messages(&self) -> bool {
        self.flags & Self::CAN_SEND_MESSAGES != 0
    }

    pub fn can_send_audios(&self) -> bool {
        self.flags & Self::CAN_SEND_AUDIOS != 0
    }

    pub fn can_send_documents(&self) -> bool {
        self.flags & Self::CAN_SEND_DOCUMENTS != 0
    }

    pub fn can_send_photos(&self) -> bool {
        self.flags & Self::CAN_SEND_PHOTOS != 0
    }

    pub fn can_send_videos(&self) -> bool {
        self.flags & Self::CAN_SEND_VIDEOS != 0
    }

    pub fn can_send_video_notes(&self) -> bool {
        self.flags & Self::CAN_SEND_VIDEO_NOTES != 0
    }

    pub fn can_send_voice_notes(&self) -> bool {
        self.flags & Self::CAN_SEND_VOICE_NOTES != 0
    }

    pub fn can_send_stickers(&self) -> bool {
        self.flags & Self::CAN_SEND_STICKERS != 0
    }

    pub fn can_send_animations(&self) -> bool {
        self.flags & Self::CAN_SEND_ANIMATIONS != 0
    }

    pub fn can_send_games(&self) -> bool {
        self.flags & Self::CAN_SEND_GAMES != 0
    }

    pub fn can_use_inline_bots(&self) -> bool {
        self.flags & Self::CAN_USE_INLINE_BOTS != 0
    }

    pub fn can_add_web_page_previews(&self) -> bool {
        self.flags & Self::CAN_ADD_WEB_PAGE_PREVIEWS != 0
    }

    pub fn can_send_polls(&self) -> bool {
        self.flags & Self::CAN_SEND_POLLS != 0
    }

    pub fn can_edit_rank(&self) -> bool {
        self.flags & Self::CAN_EDIT_RANK != 0
    }

    pub fn can_send_reactions(&self) -> bool {
        self.flags & Self::CAN_SEND_REACTIONS != 0
    }

    pub fn can_manage_linked_peers(&self) -> bool {
        self.flags & Self::CAN_MANAGE_LINKED_PEERS != 0
    }

    /// `RestrictedRights::store` (DialogParticipant.h) — `td::store(uint64)`.
    pub fn store(&self, out: &mut Vec<u8>) {
        store_u64(out, self.flags);
    }

    /// `RestrictedRights::parse` — the versioned framing plus the legacy
    /// `CAN_SEND_MEDIA` expansion into the per-media bits.
    pub fn parse_impl(parser: &mut Parser<'_>) -> Result<RestrictedRights, &'static str> {
        let mut flags = if parser.version() >= version::MAKE_PARTICIPANT_FLAGS_64_BIT {
            parser.u64_()?
        } else {
            u64::from(parser.u32_()?)
        };
        if flags & Self::LEGACY_CAN_SEND_MEDIA != 0 {
            flags |= Self::CAN_SEND_AUDIOS
                | Self::CAN_SEND_DOCUMENTS
                | Self::CAN_SEND_PHOTOS
                | Self::CAN_SEND_VIDEOS
                | Self::CAN_SEND_VIDEO_NOTES
                | Self::CAN_SEND_VOICE_NOTES;
        }
        Ok(RestrictedRights { flags })
    }
}

/// The `DialogParticipantStatus::Type` discriminator stored in bits 28-30 of
/// the flag word (`TYPE_SIZE` = 3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
enum StatusType {
    Creator = 0,
    Administrator = 1,
    Member = 2,
    Restricted = 3,
    Left = 4,
    Banned = 5,
}

/// `td::DialogParticipantStatus` (DialogParticipant.h:360): the bit-packed
/// member-status record. The rights of an administrator, member or restricted
/// member live in `flags_` (the same word the type and the `HAS_*` markers
/// occupy); `until_date_` is only meaningful for member/restricted/banned and
/// `rank_` for creator/administrator statuses.
///
/// The byte format (DialogParticipant.h:656-697, pinned against the compiled
/// C++ by `conformance/chat_participant_status_oracle`):
/// - a `uint64` flag word: the rights flags OR-ed with the type at bits 28-30,
///   `HAS_UNTIL_DATE` (bit 31) when `until_date_ > 0` and `HAS_RANK` (bit 14)
///   when the rank is non-empty;
/// - the `int32` until date and the TL-framed rank, only when flagged;
/// - the *parse* side branches on the log-event version: writers older than
///   `MakeParticipantFlags64Bit` (46) used a `uint32` flag word (the
///   64-bit-only restricted-media bits cannot appear), and any legacy
///   `LEGACY_CAN_SEND_MEDIA` bit expands into the six per-media bits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DialogParticipantStatus {
    type_: StatusType,
    until_date_: i32,
    flags_: u64,
    rank_: String,
}

impl DialogParticipantStatus {
    const HAS_RANK: u64 = 1 << 14;
    const CAN_BE_EDITED: u64 = 1 << 15;
    const IS_MEMBER: u64 = 1 << 27;
    const TYPE_SHIFT: u32 = 28;
    const TYPE_SIZE: u64 = 3;
    const HAS_UNTIL_DATE: u64 = 1 << 31;

    fn new(
        type_: StatusType,
        flags: u64,
        until_date: i32,
        rank: String,
    ) -> DialogParticipantStatus {
        DialogParticipantStatus {
            type_,
            until_date_: until_date,
            flags_: flags,
            rank_: strip_empty_characters(&rank, 16, false),
        }
    }

    /// `DialogParticipantStatus::fix_until_date`: an unreachable or negative
    /// date means "forever" (0).
    fn fix_until_date(date: i32) -> i32 {
        if date == i32::MAX || date < 0 {
            0
        } else {
            date
        }
    }

    fn get_administrator_rights(&self) -> AdministratorRights {
        AdministratorRights::from_flags(self.flags_)
    }

    fn get_restricted_rights(&self) -> RestrictedRights {
        RestrictedRights::from_flags(self.flags_)
    }

    /// `DialogParticipantStatus::Creator`.
    pub fn creator(is_member: bool, is_anonymous: bool, rank: String) -> DialogParticipantStatus {
        DialogParticipantStatus::new(
            StatusType::Creator,
            AdministratorRights::ALL_ADMINISTRATOR_RIGHTS
                | RestrictedRights::ALL_RESTRICTED_RIGHTS
                | (u64::from(is_member) * Self::IS_MEMBER)
                | (u64::from(is_anonymous) * AdministratorRights::IS_ANONYMOUS),
            0,
            rank,
        )
    }

    /// `DialogParticipantStatus::Administrator` — empty rights degrade to a
    /// plain member.
    pub fn administrator(
        administrator_rights: AdministratorRights,
        rank: String,
        can_be_edited: bool,
    ) -> DialogParticipantStatus {
        let flags = administrator_rights.flags();
        if flags == 0 {
            return DialogParticipantStatus::member(0, rank);
        }
        let flags = flags | (u64::from(can_be_edited) * Self::CAN_BE_EDITED);
        DialogParticipantStatus::new(
            StatusType::Administrator,
            Self::IS_MEMBER
                | (RestrictedRights::ALL_RESTRICTED_RIGHTS
                    & !RestrictedRights::ALL_ADMIN_PERMISSION_RIGHTS)
                | flags,
            0,
            rank,
        )
    }

    /// `DialogParticipantStatus::Member`.
    pub fn member(member_until_date: i32, rank: String) -> DialogParticipantStatus {
        DialogParticipantStatus::new(
            StatusType::Member,
            Self::IS_MEMBER | RestrictedRights::ALL_RESTRICTED_RIGHTS,
            member_until_date,
            rank,
        )
    }

    /// `DialogParticipantStatus::Restricted` — unrestricted rights or a
    /// broadcast degrade to a plain member (or `Left` without the member bit).
    pub fn restricted(
        restricted_rights: RestrictedRights,
        is_member: bool,
        restricted_until_date: i32,
        channel_type: ChannelType,
        rank: String,
    ) -> DialogParticipantStatus {
        let flags = restricted_rights.flags();
        if flags == RestrictedRights::ALL_RESTRICTED_RIGHTS
            || channel_type == ChannelType::Broadcast
        {
            return if is_member {
                DialogParticipantStatus::member(0, rank)
            } else {
                DialogParticipantStatus::left()
            };
        }
        let flags = flags | (u64::from(is_member) * Self::IS_MEMBER);
        DialogParticipantStatus::new(
            StatusType::Restricted,
            flags,
            Self::fix_until_date(restricted_until_date),
            rank,
        )
    }

    /// `DialogParticipantStatus::Left`.
    pub fn left() -> DialogParticipantStatus {
        DialogParticipantStatus::new(
            StatusType::Left,
            RestrictedRights::ALL_RESTRICTED_RIGHTS,
            0,
            String::new(),
        )
    }

    /// `DialogParticipantStatus::Banned`.
    pub fn banned(banned_until_date: i32, rank: String) -> DialogParticipantStatus {
        DialogParticipantStatus::new(
            StatusType::Banned,
            0,
            Self::fix_until_date(banned_until_date),
            rank,
        )
    }

    /// `DialogParticipantStatus::GroupAdministrator` — the legacy
    /// basic-group admin rights.
    pub fn group_administrator(
        is_current_user_creator: bool,
        rank: String,
    ) -> DialogParticipantStatus {
        DialogParticipantStatus::administrator(
            AdministratorRights::from_bools(
                false,
                true,
                true,
                false,
                false,
                true,
                true,
                true,
                true,
                false,
                false,
                true,
                false,
                false,
                false,
                false,
                true,
                false,
                true,
                ChannelType::Unknown,
            ),
            rank,
            is_current_user_creator,
        )
    }

    /// `DialogParticipantStatus::ChannelAdministrator` — the legacy
    /// supergroup/channel admin rights.
    pub fn channel_administrator(
        is_current_user_creator: bool,
        is_megagroup: bool,
    ) -> DialogParticipantStatus {
        let rights = if is_megagroup {
            AdministratorRights::from_bools(
                false,
                true,
                true,
                false,
                false,
                true,
                true,
                true,
                true,
                true,
                false,
                false,
                false,
                false,
                false,
                false,
                true,
                false,
                false,
                ChannelType::Megagroup,
            )
        } else {
            AdministratorRights::from_bools(
                false,
                true,
                false,
                true,
                true,
                true,
                false,
                true,
                false,
                false,
                false,
                false,
                true,
                true,
                true,
                true,
                false,
                false,
                false,
                ChannelType::Broadcast,
            )
        };
        DialogParticipantStatus::administrator(rights, String::new(), is_current_user_creator)
    }

    /// `DialogParticipantStatus(bool can_be_edited, chatAdminRights, string rank,
    /// ChannelType)` — forcibly returns an administrator (the
    /// `channelParticipantAdmin` intake form).
    pub(crate) fn from_channel_admin_rights(
        can_be_edited: bool,
        admin_rights: &telegram_api::ChatAdminRights,
        rank: String,
        channel_type: ChannelType,
    ) -> DialogParticipantStatus {
        DialogParticipantStatus::administrator(
            AdministratorRights::from_chat_admin_rights(Some(admin_rights), channel_type),
            rank,
            can_be_edited,
        )
    }

    /// `DialogParticipantStatus(bool is_member, chatBannedRights, ChannelType,
    /// string rank)` — forcibly returns a restricted or banned status (the
    /// `channelParticipantBanned` intake form). The C++ mutates the input's
    /// `until_date` to `i32::MAX` before building the rights so the
    /// `RestrictedRights` conversion stays quiet; the Rust copy does the same
    /// on a clone, keeping the flags and the log output identical.
    pub(crate) fn from_channel_banned_rights(
        is_member: bool,
        banned_rights: telegram_api::ChatBannedRights,
        channel_type: ChannelType,
        rank: String,
    ) -> DialogParticipantStatus {
        if banned_rights.view_messages {
            return DialogParticipantStatus::banned(banned_rights.until_date, rank);
        }
        if channel_type == ChannelType::Broadcast {
            return if is_member {
                DialogParticipantStatus::member(0, rank)
            } else {
                DialogParticipantStatus::left()
            };
        }

        let until_date = Self::fix_until_date(banned_rights.until_date);
        let mut rights = banned_rights;
        rights.until_date = i32::MAX;

        // manually create Restricted status, because the user can be restricted,
        // but with yet unknown restrictions
        let flags = RestrictedRights::from_chat_banned_rights(Some(&rights), channel_type).flags()
            | (u64::from(is_member) * Self::IS_MEMBER);
        DialogParticipantStatus::new(StatusType::Restricted, flags, until_date, rank)
    }

    pub fn has_all_administrator_rights(&self, administrator_rights: AdministratorRights) -> bool {
        let flags = administrator_rights.flags()
            & (AdministratorRights::ALL_ADMINISTRATOR_RIGHTS | AdministratorRights::IS_ANONYMOUS);
        (self.get_administrator_rights().flags() & flags) == flags
    }

    /// `DialogParticipantStatus::get_effective_restricted_rights`.
    pub fn get_effective_restricted_rights(&self) -> RestrictedRights {
        RestrictedRights::from_bools(
            self.can_send_messages(),
            self.can_send_audios(),
            self.can_send_documents(),
            self.can_send_photos(),
            self.can_send_videos(),
            self.can_send_video_notes(),
            self.can_send_voice_notes(),
            self.can_send_stickers(),
            self.can_send_animations(),
            self.can_send_games(),
            self.can_use_inline_bots(),
            self.can_add_web_page_previews(),
            self.can_send_polls(),
            self.can_change_info_and_settings(),
            self.can_invite_users(),
            self.can_pin_messages(),
            self.can_create_topics(),
            self.can_edit_rank(),
            self.can_send_reactions(),
            self.can_manage_linked_peers(),
            ChannelType::Unknown,
        )
    }

    /// `DialogParticipantStatus::apply_restrictions` — the effective flag
    /// word after the group default restrictions; the until date and rank are
    /// dropped exactly like the C++.
    pub fn apply_restrictions(
        &self,
        default_restrictions: RestrictedRights,
        is_booster: bool,
        is_bot: bool,
    ) -> DialogParticipantStatus {
        let mut flags = self.flags_;
        match self.type_ {
            StatusType::Creator => {
                // the creator can do anything and isn't affected by restrictions
            }
            StatusType::Administrator => {
                // administrators aren't affected by restrictions, but if everyone
                // can invite users, pin messages or change info, they also can
                if !is_bot {
                    flags |= default_restrictions.flags()
                        & RestrictedRights::ALL_ADMIN_PERMISSION_RIGHTS;
                }
            }
            StatusType::Member | StatusType::Restricted | StatusType::Left => {
                if !is_booster {
                    // members and restricted are affected by default restrictions
                    // unless they are supergroup boosters
                    flags &=
                        !RestrictedRights::ALL_RESTRICTED_RIGHTS | default_restrictions.flags();
                }
                if is_bot {
                    // bots must be explicitly granted administrator rights to use them
                    flags &= !RestrictedRights::ALL_ADMIN_PERMISSION_RIGHTS;
                }
            }
            StatusType::Banned => {
                // banned can do nothing even if restrictions allows them to do that
            }
        }
        DialogParticipantStatus::new(self.type_, flags, 0, String::new())
    }

    /// `DialogParticipantStatus::update_restrictions` — unrestricts when the
    /// restriction time has expired; called before every privilege check.
    pub fn update_restrictions(&mut self, global: &Global) {
        if self.until_date_ != 0 && global.unix_time() > self.until_date_ {
            self.until_date_ = 0;
            if self.type_ == StatusType::Restricted {
                self.type_ = if self.is_member() {
                    StatusType::Member
                } else {
                    StatusType::Left
                };
                self.flags_ |= RestrictedRights::ALL_RESTRICTED_RIGHTS;
            } else if self.type_ == StatusType::Banned || self.type_ == StatusType::Member {
                self.type_ = StatusType::Left;
            } else {
                debug_assert!(false, "unreachable status type in update_restrictions");
            }
        }
    }

    pub fn can_manage_dialog(&self) -> bool {
        self.get_administrator_rights().can_manage_dialog()
    }

    pub fn can_change_info_and_settings_as_administrator(&self) -> bool {
        self.get_administrator_rights()
            .can_change_info_and_settings()
    }

    pub fn can_change_info_and_settings(&self) -> bool {
        self.get_administrator_rights()
            .can_change_info_and_settings()
            || self.get_restricted_rights().can_change_info_and_settings()
    }

    pub fn can_post_messages(&self) -> bool {
        self.get_administrator_rights().can_post_messages()
    }

    pub fn can_edit_messages(&self) -> bool {
        self.get_administrator_rights().can_edit_messages()
    }

    pub fn can_delete_messages(&self) -> bool {
        self.get_administrator_rights().can_delete_messages()
    }

    pub fn can_invite_users(&self) -> bool {
        self.get_administrator_rights().can_invite_users()
            || self.get_restricted_rights().can_invite_users()
    }

    /// `DialogParticipantStatus::can_manage_invite_links` — invite links can
    /// only be managed by an administrator explicitly granted the right.
    pub fn can_manage_invite_links(&self) -> bool {
        self.get_administrator_rights().can_invite_users()
    }

    pub fn can_restrict_members(&self) -> bool {
        self.get_administrator_rights().can_restrict_members()
    }

    pub fn can_pin_messages(&self) -> bool {
        self.get_administrator_rights().can_pin_messages()
            || self.get_restricted_rights().can_pin_messages()
    }

    /// `DialogParticipantStatus::can_edit_topics` — topics can be edited only
    /// by an administrator explicitly granted the right.
    pub fn can_edit_topics(&self) -> bool {
        self.get_administrator_rights().can_manage_topics()
    }

    /// `DialogParticipantStatus::can_pin_topics` — topics can be pinned only
    /// by an administrator explicitly granted the right.
    pub fn can_pin_topics(&self) -> bool {
        self.get_administrator_rights().can_manage_topics()
    }

    pub fn can_create_topics(&self) -> bool {
        self.get_administrator_rights().can_manage_topics()
            || self.get_restricted_rights().can_manage_topics()
    }

    pub fn can_promote_members(&self) -> bool {
        self.get_administrator_rights().can_promote_members()
    }

    pub fn can_manage_calls(&self) -> bool {
        self.get_administrator_rights().can_manage_calls()
    }

    pub fn can_post_stories(&self) -> bool {
        self.get_administrator_rights().can_post_stories()
    }

    pub fn can_edit_stories(&self) -> bool {
        self.get_administrator_rights().can_edit_stories()
    }

    pub fn can_delete_stories(&self) -> bool {
        self.get_administrator_rights().can_delete_stories()
    }

    pub fn can_manage_direct_messages(&self) -> bool {
        self.get_administrator_rights().can_manage_direct_messages()
    }

    pub fn can_manage_ranks(&self) -> bool {
        self.get_administrator_rights().can_manage_ranks()
    }

    pub fn can_manage_linked_peers(&self) -> bool {
        self.get_administrator_rights().can_manage_linked_peers()
            || self.get_restricted_rights().can_manage_linked_peers()
    }

    pub fn can_manage_welcome_messages(&self) -> bool {
        self.get_administrator_rights()
            .can_manage_welcome_messages()
    }

    pub fn can_be_edited(&self) -> bool {
        (self.flags_ & Self::CAN_BE_EDITED) != 0
    }

    pub fn toggle_can_be_edited(&mut self) {
        self.flags_ ^= Self::CAN_BE_EDITED;
    }

    pub fn can_send_messages(&self) -> bool {
        self.get_restricted_rights().can_send_messages()
    }

    pub fn can_send_audios(&self) -> bool {
        self.get_restricted_rights().can_send_audios()
    }

    pub fn can_send_documents(&self) -> bool {
        self.get_restricted_rights().can_send_documents()
    }

    pub fn can_send_photos(&self) -> bool {
        self.get_restricted_rights().can_send_photos()
    }

    pub fn can_send_videos(&self) -> bool {
        self.get_restricted_rights().can_send_videos()
    }

    pub fn can_send_video_notes(&self) -> bool {
        self.get_restricted_rights().can_send_video_notes()
    }

    pub fn can_send_voice_notes(&self) -> bool {
        self.get_restricted_rights().can_send_voice_notes()
    }

    pub fn can_send_stickers(&self) -> bool {
        self.get_restricted_rights().can_send_stickers()
    }

    pub fn can_send_animations(&self) -> bool {
        self.get_restricted_rights().can_send_animations()
    }

    pub fn can_send_games(&self) -> bool {
        self.get_restricted_rights().can_send_games()
    }

    pub fn can_use_inline_bots(&self) -> bool {
        self.get_restricted_rights().can_use_inline_bots()
    }

    pub fn can_add_web_page_previews(&self) -> bool {
        self.get_restricted_rights().can_add_web_page_previews()
    }

    pub fn can_send_polls(&self) -> bool {
        self.get_restricted_rights().can_send_polls()
    }

    pub fn can_edit_rank(&self) -> bool {
        self.get_restricted_rights().can_edit_rank()
    }

    pub fn can_send_reactions(&self) -> bool {
        self.get_restricted_rights().can_send_reactions()
    }

    pub fn set_is_member(&mut self, is_member: bool) {
        if is_member {
            self.flags_ |= Self::IS_MEMBER;
        } else {
            self.flags_ &= !Self::IS_MEMBER;
        }
    }

    pub fn is_member(&self) -> bool {
        (self.flags_ & Self::IS_MEMBER) != 0
    }

    pub fn is_left(&self) -> bool {
        (self.flags_ & Self::IS_MEMBER) == 0
    }

    pub fn is_creator(&self) -> bool {
        self.type_ == StatusType::Creator
    }

    pub fn is_administrator(&self) -> bool {
        self.type_ == StatusType::Administrator || self.type_ == StatusType::Creator
    }

    pub fn is_administrator_member(&self) -> bool {
        self.type_ == StatusType::Administrator
            || (self.type_ == StatusType::Creator && self.is_member())
    }

    pub fn is_restricted(&self) -> bool {
        self.type_ == StatusType::Restricted
    }

    pub fn is_banned(&self) -> bool {
        self.type_ == StatusType::Banned
    }

    pub fn get_until_date(&self) -> i32 {
        self.until_date_
    }

    pub fn is_anonymous(&self) -> bool {
        self.get_administrator_rights().is_anonymous()
    }

    pub fn get_rank(&self) -> &str {
        &self.rank_
    }

    /// `DialogParticipantStatus::set_rank` — true when the rank changed.
    pub fn set_rank(&mut self, rank: String) -> bool {
        if self.rank_ != rank {
            self.rank_ = rank;
            true
        } else {
            false
        }
    }

    /// `DialogParticipantStatus::store` (DialogParticipant.h:656) — the bare
    /// record body; the log-event version prefix is the caller's framing.
    pub fn store(&self, out: &mut Vec<u8>) {
        let mut stored_flags = self.flags_ | ((self.type_ as i32) as u64) << Self::TYPE_SHIFT;
        if self.until_date_ > 0 {
            stored_flags |= Self::HAS_UNTIL_DATE;
        }
        if !self.rank_.is_empty() {
            stored_flags |= Self::HAS_RANK;
        }
        store_u64(out, stored_flags);
        if self.until_date_ > 0 {
            store_i32(out, self.until_date_);
        }
        if !self.rank_.is_empty() {
            store_string(out, &self.rank_);
        }
    }

    /// `DialogParticipantStatus::parse` (DialogParticipant.h:676) — the
    /// versioned reader: writers before `MakeParticipantFlags64Bit` used a
    /// `uint32` flag word, and a legacy `LEGACY_CAN_SEND_MEDIA` bit expands
    /// into the six per-media bits. Unknown type discriminants (6, 7) are
    /// rejected; the C++ would silently produce a garbage status.
    pub fn parse_impl(parser: &mut Parser<'_>) -> Result<DialogParticipantStatus, &'static str> {
        let mut stored_flags = if parser.version() >= version::MAKE_PARTICIPANT_FLAGS_64_BIT {
            parser.u64_()?
        } else {
            u64::from(parser.u32_()?)
        };
        let mut until_date = 0;
        if (stored_flags & Self::HAS_UNTIL_DATE) != 0 {
            until_date = parser.i32_()?;
            stored_flags &= !Self::HAS_UNTIL_DATE;
        }
        let mut rank = String::new();
        if (stored_flags & Self::HAS_RANK) != 0 {
            rank = parser.string()?;
            stored_flags &= !Self::HAS_RANK;
        }
        let type_ = match (stored_flags >> Self::TYPE_SHIFT) & ((1u64 << Self::TYPE_SIZE) - 1) {
            0 => StatusType::Creator,
            1 => StatusType::Administrator,
            2 => StatusType::Member,
            3 => StatusType::Restricted,
            4 => StatusType::Left,
            5 => StatusType::Banned,
            _ => return Err("Invalid DialogParticipantStatus type"),
        };
        stored_flags -= ((type_ as i32) as u64) << Self::TYPE_SHIFT;

        let mut flags = stored_flags;
        if (flags & RestrictedRights::LEGACY_CAN_SEND_MEDIA) != 0 {
            flags |= RestrictedRights::CAN_SEND_AUDIOS
                | RestrictedRights::CAN_SEND_DOCUMENTS
                | RestrictedRights::CAN_SEND_PHOTOS
                | RestrictedRights::CAN_SEND_VIDEOS
                | RestrictedRights::CAN_SEND_VIDEO_NOTES
                | RestrictedRights::CAN_SEND_VOICE_NOTES;
        }
        Ok(DialogParticipantStatus {
            type_,
            until_date_: until_date,
            flags_: flags,
            rank_: rank,
        })
    }
}

impl Default for DialogParticipantStatus {
    /// The C++ `DialogParticipantStatus()` convention: a forever-banned
    /// status without a rank.
    fn default() -> DialogParticipantStatus {
        DialogParticipantStatus::banned(0, String::new())
    }
}

/// `td::DialogParticipant` (DialogParticipant.h:721): who is in a dialog, who
/// invited them, when, and with which rights. The byte format is the four
/// fields in order — `DialogId` (`store_long`), `UserId` (`store_long`), the
/// `int32` join date, then the bare `DialogParticipantStatus` — with the
/// *parse* side branching on the log-event version: writers older than
/// `SupportBannedChannels` (31) carried the dialog as a bare `UserId`
/// (itself `int32` below `Support64BitIds`/33, `long` from 33 on), because
/// basic-group dialogs were all a `DialogId` could be back then.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DialogParticipant {
    pub dialog_id_: DialogId,
    pub inviter_user_id_: UserId,
    pub joined_date_: i32,
    pub status_: DialogParticipantStatus,
}

impl DialogParticipant {
    /// `DialogParticipant::DialogParticipant(DialogId, UserId, int32,
    /// DialogParticipantStatus)` — an invalid-but-nonzero inviter collapses to
    /// the zero user and a negative join date to 0, both logged.
    pub fn new(
        dialog_id: DialogId,
        inviter_user_id: UserId,
        joined_date: i32,
        status: DialogParticipantStatus,
    ) -> DialogParticipant {
        let mut inviter_user_id_ = inviter_user_id;
        if !inviter_user_id_.is_valid() && inviter_user_id_ != UserId::default() {
            tdutils::log_error!("Receive inviter {:?}", inviter_user_id_);
            inviter_user_id_ = UserId::default();
        }
        let mut joined_date_ = joined_date;
        if joined_date_ < 0 {
            tdutils::log_error!("Receive date {}", joined_date_);
            joined_date_ = 0;
        }
        DialogParticipant {
            dialog_id_: dialog_id,
            inviter_user_id_,
            joined_date_,
            status_: status,
        }
    }

    /// `DialogParticipant::left` — aggregate init in the C++, no fix-ups.
    pub fn left(dialog_id: DialogId) -> DialogParticipant {
        DialogParticipant {
            dialog_id_: dialog_id,
            inviter_user_id_: UserId::default(),
            joined_date_: 0,
            status_: DialogParticipantStatus::left(),
        }
    }

    /// `DialogParticipant::private_member` — a private-chat membership: the
    /// inviter is the other side when known, the member themself otherwise.
    pub fn private_member(user_id: UserId, other_user_id: UserId) -> DialogParticipant {
        let inviter_user_id = if other_user_id.is_valid() {
            other_user_id
        } else {
            user_id
        };
        DialogParticipant {
            dialog_id_: DialogId::from_user_id(user_id),
            inviter_user_id_: inviter_user_id,
            joined_date_: 0,
            status_: DialogParticipantStatus::member(0, String::new()),
        }
    }

    /// `DialogParticipant::DialogParticipant(telegram_api::ChatParticipant,
    /// int32 chat_creation_date, bool is_current_user_creator)` — the
    /// basic-group member list entries; the creator carries the chat creation
    /// date, everyone else their own join date.
    pub fn from_chat_participant(
        participant: telegram_api::ChatParticipant,
        chat_creation_date: i32,
        is_current_user_creator: bool,
    ) -> DialogParticipant {
        match participant {
            telegram_api::ChatParticipant::chatParticipant(participant) => DialogParticipant {
                dialog_id_: DialogId::from_user_id(UserId::new(participant.user_id)),
                inviter_user_id_: UserId::new(participant.inviter_id),
                joined_date_: participant.date,
                status_: DialogParticipantStatus::member(0, participant.rank),
            },
            telegram_api::ChatParticipant::chatParticipantCreator(participant) => {
                DialogParticipant {
                    dialog_id_: DialogId::from_user_id(UserId::new(participant.user_id)),
                    inviter_user_id_: UserId::new(participant.user_id),
                    joined_date_: chat_creation_date,
                    status_: DialogParticipantStatus::creator(true, false, participant.rank),
                }
            }
            telegram_api::ChatParticipant::chatParticipantAdmin(participant) => DialogParticipant {
                dialog_id_: DialogId::from_user_id(UserId::new(participant.user_id)),
                inviter_user_id_: UserId::new(participant.inviter_id),
                joined_date_: participant.date,
                status_: DialogParticipantStatus::group_administrator(
                    is_current_user_creator,
                    participant.rank,
                ),
            },
        }
    }

    /// `DialogParticipant::DialogParticipant(telegram_api::ChannelParticipant,
    /// ChannelType)` — the supergroup/broadcast member entries.
    pub fn from_channel_participant(
        participant: telegram_api::ChannelParticipant,
        channel_type: ChannelType,
    ) -> DialogParticipant {
        match participant {
            telegram_api::ChannelParticipant::channelParticipant(participant) => {
                DialogParticipant {
                    dialog_id_: DialogId::from_user_id(UserId::new(participant.user_id)),
                    inviter_user_id_: UserId::default(),
                    joined_date_: participant.date,
                    status_: DialogParticipantStatus::member(
                        participant.subscription_until_date,
                        participant.rank,
                    ),
                }
            }
            telegram_api::ChannelParticipant::channelParticipantSelf(participant) => {
                DialogParticipant {
                    dialog_id_: DialogId::from_user_id(UserId::new(participant.user_id)),
                    inviter_user_id_: UserId::new(participant.inviter_id),
                    joined_date_: participant.date,
                    status_: DialogParticipantStatus::member(
                        participant.subscription_until_date,
                        participant.rank,
                    ),
                }
            }
            telegram_api::ChannelParticipant::channelParticipantCreator(participant) => {
                DialogParticipant {
                    dialog_id_: DialogId::from_user_id(UserId::new(participant.user_id)),
                    inviter_user_id_: UserId::default(),
                    joined_date_: 0,
                    status_: DialogParticipantStatus::creator(
                        true,
                        participant.admin_rights.as_ref().unwrap().anonymous,
                        participant.rank,
                    ),
                }
            }
            telegram_api::ChannelParticipant::channelParticipantAdmin(participant) => {
                DialogParticipant {
                    dialog_id_: DialogId::from_user_id(UserId::new(participant.user_id)),
                    inviter_user_id_: UserId::new(participant.promoted_by),
                    joined_date_: participant.date,
                    status_: DialogParticipantStatus::from_channel_admin_rights(
                        participant.can_edit,
                        participant.admin_rights.as_ref().unwrap(),
                        participant.rank,
                        channel_type,
                    ),
                }
            }
            telegram_api::ChannelParticipant::channelParticipantLeft(participant) => {
                DialogParticipant {
                    dialog_id_: DialogId::from_peer(participant.peer.as_ref().unwrap()),
                    inviter_user_id_: UserId::default(),
                    joined_date_: 0,
                    status_: DialogParticipantStatus::left(),
                }
            }
            telegram_api::ChannelParticipant::channelParticipantBanned(participant) => {
                DialogParticipant {
                    dialog_id_: DialogId::from_peer(participant.peer.as_ref().unwrap()),
                    inviter_user_id_: UserId::new(participant.kicked_by),
                    joined_date_: participant.date,
                    status_: DialogParticipantStatus::from_channel_banned_rights(
                        !participant.left,
                        participant.banned_rights.unwrap(),
                        channel_type,
                        participant.rank,
                    ),
                }
            }
        }
    }

    /// `DialogParticipant::is_valid` — restricted/banned/appointed
    /// administrators must know their inviter.
    pub fn is_valid(&self) -> bool {
        if !self.dialog_id_.is_valid() || self.joined_date_ < 0 {
            return false;
        }
        if self.status_.is_restricted()
            || self.status_.is_banned()
            || (self.status_.is_administrator() && !self.status_.is_creator())
        {
            return self.inviter_user_id_.is_valid();
        }
        true
    }

    /// `DialogParticipant::store`.
    pub fn store(&self, out: &mut Vec<u8>) {
        self.dialog_id_.store(out);
        store_i64(out, self.inviter_user_id_.get());
        store_i32(out, self.joined_date_);
        self.status_.store(out);
    }

    /// `DialogParticipant::parse` — the `SupportBannedChannels` gate swaps the
    /// leading `DialogId` for the legacy bare `UserId`.
    pub fn parse_impl(parser: &mut Parser<'_>) -> Result<DialogParticipant, &'static str> {
        let dialog_id_ = if parser.version() >= version::SUPPORT_BANNED_CHANNELS {
            DialogId::parse_impl(parser)?
        } else {
            DialogId::from_user_id(parse_user_id(parser)?)
        };
        let inviter_user_id_ = parse_user_id(parser)?;
        let joined_date_ = parser.i32_()?;
        let status_ = DialogParticipantStatus::parse_impl(parser)?;
        Ok(DialogParticipant {
            dialog_id_,
            inviter_user_id_,
            joined_date_,
            status_,
        })
    }
}

/// The `td::UserId` header `parse`: `long` from `Support64BitIds`, `int32`
/// before. Shared by every record that inlines a `UserId`.
fn parse_user_id(parser: &mut Parser<'_>) -> Result<UserId, &'static str> {
    if parser.version() >= version::SUPPORT_64_BIT_IDS {
        Ok(UserId::new(parser.i64_()?))
    } else {
        Ok(UserId::new(parser.i32_()? as i64))
    }
}

impl Default for DialogParticipant {
    /// The C++ field defaults: a zero dialog, no inviter, no date, `Left`.
    fn default() -> DialogParticipant {
        DialogParticipant {
            dialog_id_: DialogId::default(),
            inviter_user_id_: UserId::default(),
            joined_date_: 0,
            status_: DialogParticipantStatus::left(),
        }
    }
}

/// `td::DialogAdministrator` (DialogAdministrator.h:21): one entry of a
/// dialog's administrator list. The `td_api::chatAdministrator` builder
/// stays with the M.5.36 read surface — it needs the UserManager's
/// `get_user_id_object`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DialogAdministrator {
    user_id_: UserId,
    rank_: String,
    is_creator_: bool,
    can_be_edited_: bool,
}

impl DialogAdministrator {
    /// `DialogAdministrator(UserId, const string &, bool, bool)`.
    pub fn new(
        user_id: UserId,
        rank: String,
        is_creator: bool,
        can_be_edited: bool,
    ) -> DialogAdministrator {
        DialogAdministrator {
            user_id_: user_id,
            rank_: rank,
            is_creator_: is_creator,
            can_be_edited_: can_be_edited,
        }
    }

    /// `DialogAdministrator(UserId, const DialogParticipantStatus &)
    /// (DialogAdministrator.cpp:16-21)` — the CHECK narrows the status to an
    /// administrator.
    pub fn from_status(user_id: UserId, status: &DialogParticipantStatus) -> DialogAdministrator {
        assert!(status.is_administrator(), "Status is not an administrator");
        DialogAdministrator {
            user_id_: user_id,
            rank_: status.get_rank().to_owned(),
            is_creator_: status.is_creator(),
            can_be_edited_: status.can_be_edited(),
        }
    }

    pub fn get_user_id(&self) -> UserId {
        self.user_id_
    }

    pub fn get_rank(&self) -> &str {
        &self.rank_
    }

    /// `DialogAdministrator::set_rank` — true when the rank changed.
    pub fn set_rank(&mut self, rank: &str) -> bool {
        if self.rank_ == rank {
            return false;
        }
        self.rank_ = rank.to_owned();
        true
    }

    pub fn is_creator(&self) -> bool {
        self.is_creator_
    }

    pub fn can_be_edited(&self) -> bool {
        self.can_be_edited_
    }

    /// `DialogAdministrator::store` (DialogAdministrator.h:60-73) — a flag
    /// block over the rank presence, the creator and the editability, then
    /// the user id and the optional rank.
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_rank = !self.rank_.is_empty();
        store_flags(out, [has_rank, self.is_creator_, self.can_be_edited_]);
        store_i64(out, self.user_id_.get());
        if has_rank {
            store_string(out, &self.rank_);
        }
    }

    /// `DialogAdministrator::parse`.
    pub fn parse_impl(parser: &mut Parser<'_>) -> Result<DialogAdministrator, &'static str> {
        let [has_rank, is_creator_, can_be_edited_] = parser.parse_flags()?;
        let user_id_ = parse_user_id(parser)?;
        let rank_ = if has_rank {
            parser.string()?
        } else {
            String::new()
        };
        Ok(DialogAdministrator {
            user_id_,
            rank_,
            is_creator_,
            can_be_edited_,
        })
    }
}

/// `td::BotCommand` (BotCommand.h:22) — a bot command as carried by
/// `BotInfo` and the `BotCommands` wrapper. The `td_api`/`InputBotCommand`
/// builders stay with their request steps.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BotCommand {
    command_: String,
    description_: String,
    is_ephemeral_: bool,
}

impl BotCommand {
    /// `BotCommand(string, string, bool)`.
    pub fn new(command: String, description: String, is_ephemeral: bool) -> BotCommand {
        BotCommand {
            command_: command,
            description_: description,
            is_ephemeral_: is_ephemeral,
        }
    }

    /// `explicit BotCommand(telegram_api::botCommand)` (BotCommand.cpp:112-117).
    pub fn from_telegram(bot_command: telegram_api::BotCommand) -> BotCommand {
        BotCommand {
            command_: bot_command.command,
            description_: bot_command.description,
            is_ephemeral_: bot_command.ephemeral,
        }
    }

    /// `BotCommand::is_ephemeral` — the flag byte of the C++ `==`/builders.
    pub fn is_ephemeral(&self) -> bool {
        self.is_ephemeral_
    }

    /// `BotCommand::store` (BotCommand.h:40-47) — the `is_ephemeral` flag
    /// block, then the two strings.
    pub fn store(&self, out: &mut Vec<u8>) {
        store_flags(out, [self.is_ephemeral_]);
        store_string(out, &self.command_);
        store_string(out, &self.description_);
    }

    /// `BotCommand::parse` — the flag block only exists from
    /// `SupportEphemeralMessages` (61).
    pub fn parse_impl(parser: &mut Parser<'_>) -> Result<BotCommand, &'static str> {
        let is_ephemeral_ = if parser.version() >= version::SUPPORT_EPHEMERAL_MESSAGES {
            parser.parse_flags::<1>()?[0]
        } else {
            false
        };
        let command_ = parser.string()?;
        let description_ = parser.string()?;
        Ok(BotCommand {
            command_,
            description_,
            is_ephemeral_,
        })
    }
}

/// `td::BotCommands` (BotCommand.h:67) — one bot's command list, as the
/// full-info records and the `botInfoChanged` intake carry them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BotCommands {
    bot_user_id_: UserId,
    commands_: Vec<BotCommand>,
}

impl BotCommands {
    /// `BotCommands::BotCommands(UserId, vector<botCommand>)`
    /// (BotCommand.cpp:131-138).
    pub fn from_telegram(
        bot_user_id: UserId,
        bot_commands: Vec<telegram_api::BotCommand>,
    ) -> BotCommands {
        BotCommands {
            bot_user_id_: bot_user_id,
            commands_: bot_commands
                .into_iter()
                .map(BotCommand::from_telegram)
                .collect(),
        }
    }

    pub fn get_bot_user_id(&self) -> UserId {
        self.bot_user_id_
    }

    /// `BotCommands::store` (BotCommand.h:86-90) — the user id then the
    /// counted vector.
    pub fn store(&self, out: &mut Vec<u8>) {
        store_i64(out, self.bot_user_id_.get());
        store_i32(out, self.commands_.len() as i32);
        for command in &self.commands_ {
            command.store(out);
        }
    }

    /// `BotCommands::parse` — the user id rides the `Support64BitIds` gate,
    /// every command its own `SupportEphemeralMessages` one. The allocation
    /// cap keeps a hostile count from aborting on start (the M.4.17 lesson);
    /// an overcount still errors out on the first missing command.
    pub fn parse_impl(parser: &mut Parser<'_>) -> Result<BotCommands, &'static str> {
        let bot_user_id_ = parse_user_id(parser)?;
        let count = parser.i32_()?;
        let mut commands_ = Vec::with_capacity(count.clamp(0, 1_000_000) as usize);
        for _ in 0..count {
            commands_.push(BotCommand::parse_impl(parser)?);
        }
        Ok(BotCommands {
            bot_user_id_,
            commands_,
        })
    }

    /// `BotCommands::update_all_bot_commands` (BotCommand.cpp:144-162) —
    /// an empty list drops the bot's entry, a changed list replaces it
    /// in place, a new bot appends; the result reports whether anything
    /// changed.
    pub fn update_all_bot_commands(
        all_bot_commands: &mut Vec<BotCommands>,
        bot_commands: BotCommands,
    ) -> bool {
        let is_from_bot =
            |commands: &BotCommands| commands.bot_user_id_ == bot_commands.bot_user_id_;

        if bot_commands.commands_.is_empty() {
            let had_any = all_bot_commands.iter().any(is_from_bot);
            all_bot_commands.retain(|commands| !is_from_bot(commands));
            return had_any;
        }
        if let Some(existing) = all_bot_commands
            .iter_mut()
            .find(|commands| is_from_bot(commands))
        {
            if *existing != bot_commands {
                *existing = bot_commands;
                return true;
            }
            return false;
        }
        all_bot_commands.push(bot_commands);
        true
    }
}

/// `td::MinChannel` (MinChannel.h:23) — the degraded channel record the
/// min-channel cache keeps (the C++ keeps min channels out of `channels_`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MinChannel {
    pub title_: String,
    pub photo_: DialogPhoto,
    pub accent_color_id_: AccentColorId,
    pub is_megagroup_: bool,
}

/// `StarManager::get_star_count` (StarManager.cpp) — the Telegram Star amount
/// sanitizer. Only the clamp is ported here; the full StarManager is its own
/// slice and this is its only in-slice caller.
pub fn get_star_count(amount: i64, allow_negative: bool) -> i64 {
    const MAX_AMOUNT: i64 = 1 << 51;
    if amount < 0 {
        if !allow_negative {
            tdutils::log_error!("Receive Telegram Star amount = {amount}");
            return 0;
        }
        if amount < -MAX_AMOUNT {
            tdutils::log_error!("Receive Telegram Star amount = {amount}");
            return -MAX_AMOUNT;
        }
    }
    if amount > MAX_AMOUNT {
        tdutils::log_error!("Receive Telegram Star amount = {amount}");
        return MAX_AMOUNT;
    }
    amount
}

/// `td::StarSubscriptionPricing` (StarSubscriptionPricing.{h,cpp,hpp}).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StarSubscriptionPricing {
    period_: i32,
    amount_: i64,
}

impl StarSubscriptionPricing {
    pub fn new(period: i32, amount: i64) -> StarSubscriptionPricing {
        StarSubscriptionPricing {
            period_: period,
            amount_: amount,
        }
    }

    /// `explicit StarSubscriptionPricing(telegram_api::object_ptr<starsSubscriptionPricing> &&)`
    /// (StarSubscriptionPricing.cpp:13-19) — a null pointer leaves the
    /// default record.
    pub fn from_telegram(pricing: Option<telegram_api::StarsSubscriptionPricing>) -> Self {
        let mut result = StarSubscriptionPricing::default();
        if let Some(pricing) = pricing {
            result.period_ = pricing.period;
            result.amount_ = get_star_count(pricing.amount, false);
        }
        result
    }

    /// `explicit StarSubscriptionPricing(td_api::object_ptr<starSubscriptionPricing> &&)`
    /// (StarSubscriptionPricing.cpp:21-29) — amounts above a billion are
    /// dropped entirely.
    pub fn from_td_api(pricing: Option<td_api::StarSubscriptionPricing>) -> Self {
        let mut result = StarSubscriptionPricing::default();
        if let Some(pricing) = pricing {
            result.period_ = pricing.period;
            result.amount_ = pricing.star_count;
            if result.amount_ > 1_000_000_000 {
                result.amount_ = 0;
            }
        }
        result
    }

    pub fn is_empty(&self) -> bool {
        self.period_ <= 0 || self.amount_ <= 0
    }

    pub fn get_period(&self) -> i32 {
        self.period_
    }

    pub fn get_amount(&self) -> i64 {
        self.amount_
    }

    /// `get_star_subscription_pricing_object`.
    pub fn get_star_subscription_pricing_object(&self) -> Option<td_api::StarSubscriptionPricing> {
        if self.is_empty() {
            return None;
        }
        Some(td_api::StarSubscriptionPricing {
            period: self.period_,
            star_count: self.amount_,
        })
    }

    /// `get_input_stars_subscription_pricing` — the request-input form; its
    /// senders land with the M.5.45 edit queries.
    pub fn get_input_stars_subscription_pricing(
        &self,
    ) -> Option<telegram_api::StarsSubscriptionPricing> {
        if self.is_empty() {
            return None;
        }
        Some(telegram_api::StarsSubscriptionPricing {
            period: self.period_,
            amount: self.amount_,
        })
    }

    /// `store` (StarSubscriptionPricing.hpp:16-22) — the (empty) flag block,
    /// then the period and the amount.
    pub fn store(&self, out: &mut Vec<u8>) {
        store_flags(out, []);
        store_i32(out, self.period_);
        store_i64(out, self.amount_);
    }

    /// `parse` (StarSubscriptionPricing.hpp:24-30).
    pub fn parse_impl(parser: &mut Parser<'_>) -> Result<Self, &'static str> {
        parser.parse_flags::<0>()?;
        let period_ = parser.i32_()?;
        let amount_ = parser.i64_()?;
        Ok(StarSubscriptionPricing { period_, amount_ })
    }
}

/// `td::DialogInviteLink` (DialogInviteLink.{h,cpp,hpp}) — one chat/channel
/// invite link, as carried by `ChatFull`/`ChannelFull` and the invite-link
/// update intakes. The `operator<<` log formatting is not ported.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DialogInviteLink {
    invite_link_: String,
    title_: String,
    creator_user_id_: UserId,
    pricing_: StarSubscriptionPricing,
    date_: i32,
    edit_date_: i32,
    expire_date_: i32,
    usage_limit_: i32,
    usage_count_: i32,
    expired_usage_count_: i32,
    request_count_: i32,
    creates_join_request_: bool,
    is_revoked_: bool,
    is_permanent_: bool,
}

impl DialogInviteLink {
    /// `DialogInviteLink(telegram_api::object_ptr<ExportedChatInvite>, bool, bool, const char *)`
    /// (DialogInviteLink.cpp:18-95). A null pointer or a
    /// `chatInvitePublicJoinRequests` (which carries no link) yields the
    /// default record; every malformed field of a `chatInviteExported` is
    /// sanitized exactly like the C++ LOG(ERROR) repair path.
    pub fn from_telegram(
        exported_invite_ptr: Option<telegram_api::ExportedChatInvite>,
        allow_truncated: bool,
        expect_join_request: bool,
        source: &str,
    ) -> Self {
        let Some(exported_invite) = exported_invite_ptr else {
            return DialogInviteLink::default();
        };
        let telegram_api::ExportedChatInvite::chatInviteExported(exported_invite) = exported_invite
        else {
            // `CHECK(get_id() == chatInvitePublicJoinRequests::ID)`, then
            // only a LOG(ERROR) — the record stays default.
            if !expect_join_request {
                tdutils::log_error!("Receive from {source} chatInvitePublicJoinRequests");
            }
            return DialogInviteLink::default();
        };

        let mut invite_link = DialogInviteLink {
            invite_link_: exported_invite.link,
            title_: exported_invite.title,
            creator_user_id_: UserId::new(exported_invite.admin_id),
            pricing_: StarSubscriptionPricing::from_telegram(exported_invite.subscription_pricing),
            date_: exported_invite.date,
            expire_date_: exported_invite.expire_date,
            usage_limit_: exported_invite.usage_limit,
            usage_count_: exported_invite.usage,
            expired_usage_count_: exported_invite.subscription_expired,
            edit_date_: exported_invite.start_date,
            request_count_: exported_invite.requested,
            creates_join_request_: exported_invite.request_needed,
            is_revoked_: exported_invite.revoked,
            is_permanent_: exported_invite.permanent,
        };

        let full_source = format!("invite link {} from {}", invite_link.invite_link_, source);
        if !Self::is_valid_invite_link(&invite_link.invite_link_, allow_truncated) {
            tdutils::log_error!("Unsupported {full_source}");
        }
        if !invite_link.creator_user_id_.is_valid() {
            tdutils::log_error!("Receive invalid creator of invite link {full_source}");
            invite_link.creator_user_id_ = UserId::default();
        }
        if invite_link.date_ != 0 && invite_link.date_ < 1_000_000_000 {
            tdutils::log_error!("Receive wrong date as a creation date of {full_source}");
            invite_link.date_ = 0;
        }
        if invite_link.expire_date_ != 0 && invite_link.expire_date_ < 1_000_000_000 {
            tdutils::log_error!("Receive wrong date as an expire date of {full_source}");
            invite_link.expire_date_ = 0;
        }
        if invite_link.usage_limit_ < 0 {
            tdutils::log_error!("Receive wrong usage limit for {full_source}");
            invite_link.usage_limit_ = 0;
        }
        if invite_link.usage_count_ < 0 {
            tdutils::log_error!("Receive wrong usage count for {full_source}");
            invite_link.usage_count_ = 0;
        }
        if invite_link.expired_usage_count_ < 0 {
            tdutils::log_error!("Receive wrong expired usage count for {full_source}");
            invite_link.expired_usage_count_ = 0;
        }
        if invite_link.edit_date_ != 0 && invite_link.edit_date_ < 1_000_000_000 {
            tdutils::log_error!("Receive wrong date as an edit date of {full_source}");
            invite_link.edit_date_ = 0;
        }
        if invite_link.request_count_ < 0 {
            tdutils::log_error!("Receive wrong pending join request count for {full_source}");
            invite_link.request_count_ = 0;
        }

        // fb6203c77: primary links can carry pending join requests now, so
        // request_count_ is no longer wiped with the other primary-only
        // fields.
        if invite_link.is_permanent_
            && (!invite_link.title_.is_empty()
                || invite_link.expire_date_ > 0
                || invite_link.usage_limit_ > 0
                || invite_link.edit_date_ > 0)
        {
            tdutils::log_error!("Receive wrong primary {full_source}");
            invite_link.title_.clear();
            invite_link.expire_date_ = 0;
            invite_link.usage_limit_ = 0;
            invite_link.edit_date_ = 0;
        }
        if invite_link.creates_join_request_ && invite_link.usage_limit_ > 0 {
            tdutils::log_error!("Receive wrong {full_source}");
            invite_link.usage_limit_ = 0;
        }
        invite_link
    }

    /// `DialogInviteLink::is_valid_invite_link` (DialogInviteLink.cpp:97-102).
    /// The LinkManager URL stack it consults (`get_dialog_invite_link_hash`)
    /// lands with the LinkManager slice — in this slice the check only gates
    /// a LOG(ERROR) (DialogInviteLink.cpp:48), so it is a documented
    /// always-valid stand-in.
    pub fn is_valid_invite_link(invite_link: &str, allow_truncated: bool) -> bool {
        let _ = (invite_link, allow_truncated);
        true
    }

    /// `is_valid`.
    pub fn is_valid(&self) -> bool {
        !self.invite_link_.is_empty() && self.creator_user_id_.is_valid() && self.date_ > 0
    }

    pub fn is_permanent(&self) -> bool {
        self.is_permanent_
    }

    pub fn get_invite_link(&self) -> &str {
        &self.invite_link_
    }

    pub fn get_creator_user_id(&self) -> UserId {
        self.creator_user_id_
    }

    /// `get_chat_invite_link_object` (DialogInviteLink.cpp:104-115) — `None`
    /// where the C++ returns a null object.
    pub fn get_chat_invite_link_object(
        &self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
    ) -> Option<td_api::ChatInviteLink> {
        if !self.is_valid() {
            return None;
        }
        Some(td_api::ChatInviteLink {
            invite_link: self.invite_link_.clone(),
            name: self.title_.clone(),
            creator_user_id: user_manager.get_user_id_object(
                env,
                self.creator_user_id_,
                "get_chat_invite_link_object",
            ),
            date: self.date_,
            edit_date: self.edit_date_,
            expiration_date: self.expire_date_,
            // The C++ passes a possibly-null object_ptr; the generated Rust
            // field is the Option mirroring it (empty pricing → None).
            subscription_pricing: self.pricing_.get_star_subscription_pricing_object(),
            member_limit: self.usage_limit_,
            member_count: self.usage_count_,
            expired_member_count: self.expired_usage_count_,
            pending_join_request_count: self.request_count_,
            creates_join_request: self.creates_join_request_,
            is_primary: self.is_permanent_,
            is_revoked: self.is_revoked_,
        })
    }

    /// `store` (DialogInviteLink.hpp:17-68).
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_expire_date = self.expire_date_ != 0;
        let has_usage_limit = self.usage_limit_ != 0;
        let has_usage_count = self.usage_count_ != 0;
        let has_edit_date = self.edit_date_ != 0;
        let has_request_count = self.request_count_ != 0;
        let has_title = !self.title_.is_empty();
        let has_pricing = !self.pricing_.is_empty();
        let has_expired_usage_count = self.expired_usage_count_ != 0;
        store_flags(
            out,
            [
                self.is_revoked_,
                self.is_permanent_,
                has_expire_date,
                has_usage_limit,
                has_usage_count,
                has_edit_date,
                has_request_count,
                self.creates_join_request_,
                has_title,
                has_pricing,
                has_expired_usage_count,
            ],
        );
        store_string(out, &self.invite_link_);
        store_i64(out, self.creator_user_id_.get());
        store_i32(out, self.date_);
        if has_expire_date {
            store_i32(out, self.expire_date_);
        }
        if has_usage_limit {
            store_i32(out, self.usage_limit_);
        }
        if has_usage_count {
            store_i32(out, self.usage_count_);
        }
        if has_edit_date {
            store_i32(out, self.edit_date_);
        }
        if has_request_count {
            store_i32(out, self.request_count_);
        }
        if has_title {
            store_string(out, &self.title_);
        }
        if has_pricing {
            self.pricing_.store(out);
        }
        if has_expired_usage_count {
            store_i32(out, self.expired_usage_count_);
        }
    }

    /// `parse` (DialogInviteLink.hpp:70-124), including the trailing
    /// `creates_join_request` usage-limit reset.
    pub fn parse_impl(parser: &mut Parser<'_>) -> Result<Self, &'static str> {
        let [
            is_revoked_,
            is_permanent_,
            has_expire_date,
            has_usage_limit,
            has_usage_count,
            has_edit_date,
            has_request_count,
            creates_join_request_,
            has_title,
            has_pricing,
            has_expired_usage_count,
        ] = parser.parse_flags()?;
        let invite_link_ = parser.string()?;
        let creator_user_id_ = parse_user_id(parser)?;
        let date_ = parser.i32_()?;
        let expire_date_ = if has_expire_date { parser.i32_()? } else { 0 };
        let usage_limit_ = if has_usage_limit { parser.i32_()? } else { 0 };
        let usage_count_ = if has_usage_count { parser.i32_()? } else { 0 };
        let edit_date_ = if has_edit_date { parser.i32_()? } else { 0 };
        let request_count_ = if has_request_count { parser.i32_()? } else { 0 };
        let title_ = if has_title {
            parser.string()?
        } else {
            String::new()
        };
        let pricing_ = if has_pricing {
            StarSubscriptionPricing::parse_impl(parser)?
        } else {
            StarSubscriptionPricing::default()
        };
        let expired_usage_count_ = if has_expired_usage_count {
            parser.i32_()?
        } else {
            0
        };
        let mut invite_link = DialogInviteLink {
            invite_link_,
            title_,
            creator_user_id_,
            pricing_,
            date_,
            edit_date_,
            expire_date_,
            usage_limit_,
            usage_count_,
            expired_usage_count_,
            request_count_,
            creates_join_request_,
            is_revoked_,
            is_permanent_,
        };
        if creates_join_request_ {
            invite_link.usage_limit_ = 0;
        }
        Ok(invite_link)
    }
}

/// `td::MissingInvitee` (MissingInvitee.{h,cpp}) — one entry of a failed
/// members add. The td_api builders take the UserManager like the C++.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MissingInvitee {
    user_id_: UserId,
    premium_would_allow_invite_: bool,
    premium_required_for_pm_: bool,
}

impl MissingInvitee {
    /// `explicit MissingInvitee(telegram_api::object_ptr<missingInvitee> &&)`
    /// (MissingInvitee.cpp:17-21).
    pub fn from_telegram(invitee: telegram_api::MissingInvitee) -> Self {
        MissingInvitee {
            user_id_: UserId::new(invitee.user_id),
            premium_would_allow_invite_: invitee.premium_would_allow_invite,
            premium_required_for_pm_: invitee.premium_required_for_pm,
        }
    }

    pub fn is_valid(&self) -> bool {
        self.user_id_.is_valid()
    }

    pub fn get_user_id(&self) -> UserId {
        self.user_id_
    }

    /// `get_failed_to_add_member_object` (MissingInvitee.cpp:23-29).
    pub fn get_failed_to_add_member_object(
        &self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
    ) -> td_api::FailedToAddMember {
        td_api::FailedToAddMember {
            user_id: user_manager.get_user_id_object(
                env,
                self.user_id_,
                "get_failed_to_add_member_object",
            ),
            premium_would_allow_invite: self.premium_would_allow_invite_,
            premium_required_to_send_messages: self.premium_required_for_pm_,
        }
    }
}

/// `td::MissingInvitees` (MissingInvitee.{h,cpp}).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MissingInvitees {
    missing_invitees_: Vec<MissingInvitee>,
}

impl MissingInvitees {
    /// `explicit MissingInvitees(vector<object_ptr<missingInvitee>> &&)`
    /// (MissingInvitee.cpp:36-44) — invalid entries are logged and dropped.
    pub fn from_telegram(invitees: Vec<telegram_api::MissingInvitee>) -> Self {
        let mut missing_invitees = Vec::new();
        for invitee in invitees {
            let invitee = MissingInvitee::from_telegram(invitee);
            if !invitee.is_valid() {
                tdutils::log_error!("Receive invalid missing invitee");
                continue;
            }
            missing_invitees.push(invitee);
        }
        MissingInvitees {
            missing_invitees_: missing_invitees,
        }
    }

    /// `get_failed_to_add_members_object` (MissingInvitee.cpp:46-52).
    pub fn get_failed_to_add_members_object(
        &self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
    ) -> td_api::FailedToAddMembers {
        td_api::FailedToAddMembers {
            failed_to_add_members: self
                .missing_invitees_
                .iter()
                .map(|invitee| invitee.get_failed_to_add_member_object(user_manager, env))
                .collect(),
        }
    }
}

/// `get_verification_status_object` (VerificationStatus.cpp:11-18) — `None`
/// where the C++ returns a null object. The C++ `Td *td` parameter is unused.
pub fn get_verification_status_object(
    is_verified: bool,
    is_scam: bool,
    is_fake: bool,
    bot_verification_custom_emoji_id: CustomEmojiId,
) -> Option<td_api::VerificationStatus> {
    if !is_verified && !is_scam && !is_fake && !bot_verification_custom_emoji_id.is_valid() {
        return None;
    }
    Some(td_api::VerificationStatus {
        is_verified,
        is_scam,
        is_fake,
        bot_verification_icon_custom_emoji_id: bot_verification_custom_emoji_id.get(),
    })
}

/// The `ChatId` header `parse`: `long` from `Support64BitIds`, `int32`
/// before; the header `store` is always `store_long`.
fn parse_chat_id(parser: &mut Parser<'_>) -> Result<ChatId, &'static str> {
    if parser.version() >= version::SUPPORT_64_BIT_IDS {
        Ok(ChatId::new(parser.i64_()?))
    } else {
        Ok(ChatId::new(parser.i32_()? as i64))
    }
}

/// The `ChannelId` header `parse`, symmetric to [`parse_chat_id`].
fn parse_channel_id(parser: &mut Parser<'_>) -> Result<ChannelId, &'static str> {
    if parser.version() >= version::SUPPORT_64_BIT_IDS {
        Ok(ChannelId::new(parser.i64_()?))
    } else {
        Ok(ChannelId::new(parser.i32_()? as i64))
    }
}

/// `td::Location` (Location.h:20-134) — the geolocation record. Only the
/// value semantics and the persistence land here; the constructors and the
/// object builders need the geo-point accessors and land with their
/// consumers.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Location {
    is_empty_: bool,
    latitude_: f64,
    longitude_: f64,
    horizontal_accuracy_: f64,
    access_hash_: i64,
}

impl Default for Location {
    fn default() -> Self {
        Location {
            is_empty_: true,
            latitude_: 0.0,
            longitude_: 0.0,
            horizontal_accuracy_: 0.0,
            access_hash_: 0,
        }
    }
}

impl Location {
    /// `Location::fix_accuracy` (Location.cpp:14-21).
    fn fix_accuracy(accuracy: f64) -> f64 {
        if !accuracy.is_finite() || accuracy <= 0.0 {
            return 0.0;
        }
        if accuracy >= 1500.0 {
            return 1500.0;
        }
        accuracy
    }

    /// `Location::init` + the `geoPoint` constructor (Location.cpp:23-40,
    /// 48-62). The `G()->add_location_access_hash` fan-out is the
    /// Files-step stub.
    pub fn from_geo_point(geo_point: telegram_api::GeoPoint) -> Location {
        let mut result = Location::default();
        if let telegram_api::GeoPoint::geoPoint(geo_point) = geo_point {
            let latitude = geo_point.lat;
            let longitude = geo_point.long;
            if latitude.is_finite()
                && longitude.is_finite()
                && latitude.abs() <= 90.0
                && longitude.abs() <= 180.0
            {
                result.is_empty_ = false;
                result.latitude_ = latitude;
                result.longitude_ = longitude;
                result.horizontal_accuracy_ = Self::fix_accuracy(geo_point.accuracy_radius as f64);
                result.access_hash_ = geo_point.access_hash;
            }
        }
        result
    }

    /// `Location::empty` — the C++ `is_empty_` flag.
    pub fn is_empty(&self) -> bool {
        self.is_empty_
    }

    /// `Location::store` (Location.h:80-99).
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_access_hash = self.access_hash_ != 0;
        let has_horizontal_accuracy = self.horizontal_accuracy_ > 0.0;
        store_flags(
            out,
            [self.is_empty_, has_access_hash, has_horizontal_accuracy],
        );
        store_f64(out, self.latitude_);
        store_f64(out, self.longitude_);
        if has_access_hash {
            store_i64(out, self.access_hash_);
        }
        if has_horizontal_accuracy {
            store_f64(out, self.horizontal_accuracy_);
        }
    }

    /// `Location::parse` (Location.h:101-134).
    pub fn parse_impl(parser: &mut Parser<'_>) -> Result<Self, &'static str> {
        let [is_empty_, has_access_hash, has_horizontal_accuracy] = parser.parse_flags()?;
        let latitude_ = parser.f64_()?;
        let longitude_ = parser.f64_()?;
        let access_hash_ = if has_access_hash { parser.i64_()? } else { 0 };
        let horizontal_accuracy_ = if has_horizontal_accuracy {
            parser.f64_()?
        } else {
            0.0
        };
        Ok(Location {
            is_empty_,
            latitude_,
            longitude_,
            horizontal_accuracy_,
            access_hash_,
        })
    }

    /// `Location::get_latitude` (Location.h:62-64).
    pub fn get_latitude(&self) -> f64 {
        self.latitude_
    }

    /// `Location::get_longitude` (Location.h:66-68).
    pub fn get_longitude(&self) -> f64 {
        self.longitude_
    }

    /// `Location::Location(td_api::location)` (Location.cpp:66-72) —
    /// the access hash is a Files-step stub (0), like the C++ nullptr Td.
    pub fn from_td_api_location(location: &td_api::Location) -> Location {
        let mut result = Location::default();
        if location.latitude.is_finite()
            && location.longitude.is_finite()
            && location.latitude.abs() <= 90.0
            && location.longitude.abs() <= 180.0
        {
            result.is_empty_ = false;
            result.latitude_ = location.latitude;
            result.longitude_ = location.longitude;
            result.horizontal_accuracy_ = Self::fix_accuracy(location.horizontal_accuracy);
            result.access_hash_ = 0;
        }
        result
    }

    /// `Location::is_valid_map_point` (Location.cpp:78-81).
    pub fn is_valid_map_point(&self) -> bool {
        const MAX_VALID_MAP_LATITUDE: f64 = 85.05112877;
        !self.is_empty() && self.latitude_.abs() <= MAX_VALID_MAP_LATITUDE
    }

    /// `Location::get_location_object` (Location.cpp:83-88).
    pub fn get_location_object(&self) -> Option<td_api::Location> {
        if self.is_empty() {
            return None;
        }
        Some(td_api::Location {
            latitude: self.latitude_,
            longitude: self.longitude_,
            horizontal_accuracy: self.horizontal_accuracy_,
        })
    }

    /// `Location::get_input_geo_point` (Location.cpp:90-102).
    pub fn get_input_geo_point(&self) -> telegram_api::InputGeoPoint {
        if self.is_empty() {
            return telegram_api::InputGeoPoint::inputGeoPointEmpty(
                telegram_api::inputGeoPointEmpty {},
            );
        }
        let mut flags = 0;
        if self.horizontal_accuracy_ > 0.0 {
            // inputGeoPoint::ACCURACY_RADIUS_MASK
            flags |= 1 << 0;
        }
        telegram_api::InputGeoPoint::inputGeoPoint(telegram_api::inputGeoPoint {
            flags,
            lat: self.latitude_,
            long: self.longitude_,
            accuracy_radius: self.horizontal_accuracy_.ceil() as i32,
        })
    }
}

/// `td::DialogLocation` (DialogLocation.{h,cpp}) — a chat/channel location
/// with its textual address. The constructors need the geo-point accessors
/// and land with the channel-location intake (M.5.25/M.5.26).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DialogLocation {
    location_: Location,
    address_: String,
}

impl DialogLocation {
    /// `DialogLocation::DialogLocation(telegram_api::ChannelLocation)`
    /// (DialogLocation.cpp:13-19) — only `channelLocation` carries data;
    /// the empty and address-only constructors collapse to the default.
    pub fn from_telegram(channel_location: telegram_api::ChannelLocation) -> DialogLocation {
        match channel_location {
            telegram_api::ChannelLocation::channelLocation(l) => DialogLocation {
                location_: Location::from_geo_point(l.geo_point.unwrap()),
                address_: l.address,
            },
            _ => DialogLocation::default(),
        }
    }

    /// `DialogLocation::empty` (DialogLocation.cpp).
    pub fn empty(&self) -> bool {
        self.location_.is_empty() && self.address_.is_empty()
    }

    /// `DialogLocation(businessLocation)` (DialogLocation.cpp:38-45, M.48)
    /// — the location record + the cleaned address.
    pub fn from_business_location(
        business_location: Option<td_api::BusinessLocation>,
    ) -> DialogLocation {
        let mut result = DialogLocation::default();
        let Some(business_location) = business_location else {
            return result;
        };
        if let Some(l) = business_location.location {
            result.location_ = Location::from_geo_point(telegram_api::GeoPoint::geoPoint(
                telegram_api::geoPoint {
                    flags: 0,
                    lat: l.latitude,
                    long: l.longitude,
                    accuracy_radius: l.horizontal_accuracy as i32,
                    access_hash: 0,
                },
            ));
        }
        let mut address = business_location.address;
        if crate::misc::clean_input_string(&address).is_none() {
            address.clear();
        }
        result.address_ = address;
        result
    }

    /// `as_dialog_location` (Td.cpp conversions) — the td_api chatLocation
    /// into the internal record. The access hash is a Files-step stub (0).
    pub fn from_td_api(chat_location: &td_api::ChatLocation) -> DialogLocation {
        let mut result = DialogLocation::default();
        let l = chat_location.location.as_ref().unwrap();
        result.location_ =
            Location::from_geo_point(telegram_api::GeoPoint::geoPoint(telegram_api::geoPoint {
                flags: 0,
                lat: l.latitude,
                long: l.longitude,
                accuracy_radius: l.horizontal_accuracy as i32,
                access_hash: 0,
            }));
        result.address_ = chat_location.address.clone();
        result
    }

    /// `get_address`.
    pub fn get_address(&self) -> &str {
        &self.address_
    }

    /// `store` (DialogLocation.h:51-55).
    pub fn store(&self, out: &mut Vec<u8>) {
        self.location_.store(out);
        store_string(out, &self.address_);
    }

    /// `parse` (DialogLocation.h:57-62).
    pub fn parse_impl(parser: &mut Parser<'_>) -> Result<Self, &'static str> {
        let location_ = Location::parse_impl(parser)?;
        let address_ = parser.string()?;
        Ok(DialogLocation {
            location_,
            address_,
        })
    }
}

/// `td::ProfileTab` (ProfileTab.h:19) — the main profile tab of a
/// channel. The `get_profile_tab`/`get_input_profile_tab` conversions and
/// the td_api builders land with the M.5.39/M.5.40 setter steps.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProfileTab {
    #[default]
    Default,
    Posts,
    Gifts,
    Media,
    Files,
    Music,
    Voice,
    Links,
    Gifs,
}

impl ProfileTab {
    /// The persisted form: `store(static_cast<int32>)` via the generic enum
    /// store. A value outside the enumeration can only come from a corrupt
    /// database — the C++ keeps it as the raw int; here it falls back to
    /// `Default`.
    pub fn from_raw(value: i32) -> ProfileTab {
        match value {
            0 => ProfileTab::Default,
            1 => ProfileTab::Posts,
            2 => ProfileTab::Gifts,
            3 => ProfileTab::Media,
            4 => ProfileTab::Files,
            5 => ProfileTab::Music,
            6 => ProfileTab::Voice,
            7 => ProfileTab::Links,
            8 => ProfileTab::Gifs,
            _ => {
                tdutils::log_error!("Receive invalid ProfileTab {value}");
                ProfileTab::Default
            }
        }
    }

    pub fn as_raw(self) -> i32 {
        self as i32
    }
}

/// `td::BotVerification` (BotVerification.{h,cpp,hpp}) — the verification
/// stamp a bot puts on a channel. The td_api builder needs the entity
/// parser (MessageEntity step) and lands with the channel-full intake.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BotVerification {
    bot_user_id_: UserId,
    icon_: CustomEmojiId,
    description_: crate::message_entity::FormattedText,
}

impl BotVerification {
    /// `BotVerification(telegram_api::object_ptr<botVerification> &&)`
    /// (BotVerification.cpp:18-25).
    pub fn from_telegram(bot_verification: telegram_api::BotVerification) -> Self {
        BotVerification {
            bot_user_id_: UserId::new(bot_verification.bot_id),
            icon_: CustomEmojiId::new(bot_verification.icon),
            description_: crate::message_entity::get_formatted_text_of_text_with_entities::<()>(
                None,
                bot_verification.description,
                true,
                false,
                "BotVerification",
            ),
        }
    }

    /// `BotVerification::get_bot_verification` (BotVerification.cpp:27-38) —
    /// `None` for a null pointer and for an invalid record (the C++ logs
    /// and returns nullptr).
    pub fn get_bot_verification(
        bot_verification: Option<telegram_api::BotVerification>,
    ) -> Option<Self> {
        let bot_verification = BotVerification::from_telegram(bot_verification?);
        if !bot_verification.is_valid() {
            tdutils::log_error!("Receive invalid bot verification");
            return None;
        }
        Some(bot_verification)
    }

    /// `is_valid` (BotVerification.h:33-35).
    pub fn is_valid(&self) -> bool {
        self.bot_user_id_.is_valid() && self.icon_.is_valid()
    }

    /// `BotVerification::get_bot_verification_object`
    /// (BotVerification.cpp:76-83). The description's entity search is the
    /// MessageEntity step (M.8) — the text answers with no entities.
    pub(crate) fn get_bot_verification_object(
        &self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
    ) -> td_api::BotVerification {
        let bot_user_id =
            user_manager.get_user_id_object(env, self.bot_user_id_, "botVerification");
        let description = crate::message_entity::get_formatted_text_object::<()>(
            None,
            &self.description_,
            true,
            -1,
        );
        td_api::BotVerification {
            bot_user_id,
            icon_custom_emoji_id: self.icon_.get(),
            custom_description: Some(description),
        }
    }

    /// `store` (BotVerification.hpp:17-29) — the entity-presence flag,
    /// then the bot user id, the icon and the description text (+entities).
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_description_entities = !self.description_.entities.is_empty();
        store_flags(out, [has_description_entities]);
        store_i64(out, self.bot_user_id_.get());
        store_i64(out, self.icon_.get());
        store_string(out, &self.description_.text);
        if has_description_entities {
            for entity in &self.description_.entities {
                entity.store(out);
            }
        }
    }

    /// `parse` (BotVerification.hpp:31-48).
    pub fn parse_impl(parser: &mut Parser<'_>) -> Result<Self, &'static str> {
        let [has_description_entities] = parser.parse_flags()?;
        let bot_user_id_ = parse_user_id(parser)?;
        let icon_ = CustomEmojiId::new(parser.i64_()?);
        let mut description_ = crate::message_entity::FormattedText::default();
        description_.text = parser.string()?;
        if has_description_entities {
            let len = parser.i32_()?;
            if len < 0 || parser.left_len() < len as usize {
                return Err("Wrong vector length");
            }
            for _ in 0..len {
                description_
                    .entities
                    .push(crate::message_entity::MessageEntity::parse(parser)?);
            }
        }
        Ok(BotVerification {
            bot_user_id_,
            icon_,
            description_,
        })
    }
}

/// `ChatManager::Chat` (ChatManager.h:444-497) — one basic group.
#[derive(Clone, Debug, PartialEq)]
pub struct Chat {
    pub title: String,
    pub photo: DialogPhoto,
    pub participant_count: i32,
    pub date: i32,
    pub version: i32,
    pub default_permissions_version: i32,
    pub pinned_message_version: i32,
    pub migrated_to_channel_id: ChannelId,

    pub status: DialogParticipantStatus,
    pub default_permissions: RestrictedRights,

    pub cache_version: u32,

    pub is_active: bool,
    pub noforwards: bool,

    pub is_title_changed: bool,
    pub is_photo_changed: bool,
    pub is_default_permissions_changed: bool,
    pub is_status_changed: bool,
    pub is_is_active_changed: bool,
    pub is_noforwards_changed: bool,
    pub is_being_updated: bool,
    /// have new changes that need to be sent to the client and database
    pub is_changed: bool,
    /// have new changes that need only to be saved to the database
    pub need_save_to_database: bool,
    pub is_update_basic_group_sent: bool,

    /// whether cached value is rechecked
    pub is_repaired: bool,

    /// is current chat version being saved/is saved to the database
    pub is_saved: bool,
    pub is_being_saved: bool,

    /// true, if the chat was received from the server and not the database
    pub is_received_from_server: bool,

    pub log_event_id: u64,
}

impl Chat {
    pub const CACHE_VERSION: u32 = 5;

    pub fn new() -> Chat {
        Chat {
            title: String::new(),
            photo: DialogPhoto::default(),
            participant_count: 0,
            date: 0,
            version: -1,
            default_permissions_version: -1,
            pinned_message_version: -1,
            migrated_to_channel_id: ChannelId::default(),
            status: DialogParticipantStatus::banned(0, String::new()),
            default_permissions: RestrictedRights::restrict_all(),
            cache_version: 0,
            is_active: false,
            noforwards: false,
            is_title_changed: true,
            is_photo_changed: true,
            is_default_permissions_changed: true,
            is_status_changed: true,
            is_is_active_changed: true,
            is_noforwards_changed: true,
            is_being_updated: false,
            is_changed: true,
            need_save_to_database: true,
            is_update_basic_group_sent: false,
            is_repaired: false,
            is_saved: false,
            is_being_saved: false,
            is_received_from_server: false,
            log_event_id: 0,
        }
    }

    /// `Chat::store` (ChatManager.cpp:1830-1872) with the photo persistence
    /// deferred (`has_photo` forced `false`, the User::store convention).
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_photo = false; // DIVERGENCE(Files step): photo.small_file_id.is_valid()
        let use_new_rights = true;
        let has_default_permissions_version = self.default_permissions_version != -1;
        let has_pinned_message_version = self.pinned_message_version != -1;
        let has_cache_version = self.cache_version != 0;
        store_flags(
            out,
            [
                false,
                false,
                false,
                false,
                false,
                false,
                self.is_active,
                has_photo,
                use_new_rights,
                has_default_permissions_version,
                has_pinned_message_version,
                has_cache_version,
                self.noforwards,
            ],
        );

        store_string(out, &self.title);
        // if (has_photo) { store(photo, storer); } — Files step
        store_i32(out, self.participant_count);
        store_i32(out, self.date);
        store_i64(out, self.migrated_to_channel_id.get());
        store_i32(out, self.version);
        self.status.store(out);
        self.default_permissions.store(out);
        if has_default_permissions_version {
            store_i32(out, self.default_permissions_version);
        }
        if has_pinned_message_version {
            store_i32(out, self.pinned_message_version);
        }
        if has_cache_version {
            store_u32(out, self.cache_version);
        }
    }

    /// `log_event_store(Chat)` — the version-prefixed database value.
    pub fn store_value(&self, out: &mut Vec<u8>) {
        crate::store_parse::store_log_event_version(out);
        self.store(out);
    }

    /// `Chat::parse` (ChatManager.cpp:1874-1957), including the legacy
    /// rights reconstruction and the double administrator-rank downgrade.
    pub fn parse_value(input: &[u8]) -> Result<Chat, &'static str> {
        let mut parser = Parser::new_log_event(input)?;
        let chat = Chat::parse_impl(&mut parser)?;
        parser.fetch_end()?;
        Ok(chat)
    }

    pub fn parse_impl(parser: &mut Parser<'_>) -> Result<Chat, &'static str> {
        let [
            left,
            kicked,
            is_creator,
            is_administrator,
            everyone_is_administrator,
            can_edit,
            is_active,
            has_photo,
            use_new_rights,
            has_default_permissions_version,
            has_pinned_message_version,
            has_cache_version,
            noforwards,
        ] = parser.parse_flags()?;
        if has_photo {
            return Err("Chat photo needs the FileManager format (Files step)");
        }

        let mut chat = Chat::new();
        chat.is_active = is_active;
        chat.noforwards = noforwards;
        chat.title = parser.string()?;
        // if (has_photo) { parse(photo, parser); } — rejected above
        chat.participant_count = parser.i32_()?;
        chat.date = parser.i32_()?;
        chat.migrated_to_channel_id = parse_channel_id(parser)?;
        chat.version = parser.i32_()?;
        if use_new_rights {
            chat.status = DialogParticipantStatus::parse_impl(parser)?;
            chat.default_permissions = RestrictedRights::parse_impl(parser)?;
            if chat.status.is_administrator() && !chat.status.is_creator() {
                chat.status = DialogParticipantStatus::group_administrator(
                    false,
                    chat.status.get_rank().to_owned(),
                );
            }
        } else {
            if can_edit != (is_creator || is_administrator || everyone_is_administrator) {
                tdutils::log_error!("Have wrong can_edit flag");
            }

            if kicked || !is_active {
                chat.status = DialogParticipantStatus::banned(0, String::new());
            } else if left {
                chat.status = DialogParticipantStatus::left();
            } else if is_creator {
                chat.status = DialogParticipantStatus::creator(true, false, String::new());
            } else if is_administrator && !everyone_is_administrator {
                chat.status = DialogParticipantStatus::group_administrator(false, String::new());
            } else {
                chat.status = DialogParticipantStatus::member(0, String::new());
            }
            chat.default_permissions = RestrictedRights::from_bools(
                true,
                true,
                true,
                true,
                true,
                true,
                true,
                true,
                true,
                true,
                true,
                true,
                true,
                everyone_is_administrator,
                everyone_is_administrator,
                everyone_is_administrator,
                false,
                false,
                true,
                true,
                ChannelType::Unknown,
            );
        }
        if has_default_permissions_version {
            chat.default_permissions_version = parser.i32_()?;
        }
        if has_pinned_message_version {
            chat.pinned_message_version = parser.i32_()?;
        }
        if has_cache_version {
            chat.cache_version = parser.u32_()?;
        }

        if !tdutils::utf8::check_utf8(chat.title.as_bytes()) {
            tdutils::log_error!("Have invalid chat title");
            chat.title.clear();
            chat.cache_version = 0;
        }

        if chat.status.is_administrator() && !chat.status.is_creator() {
            chat.status = DialogParticipantStatus::group_administrator(false, String::new());
        }
        Ok(chat)
    }
}

impl Default for Chat {
    fn default() -> Self {
        Chat::new()
    }
}

/// `ChatManager::ChatFull` (ChatManager.h:499-545) — one basic group's
/// full info.
#[derive(Clone, Debug, PartialEq)]
pub struct ChatFull {
    pub version: i32,
    pub creator_user_id: UserId,
    pub participants: Vec<DialogParticipant>,

    pub photo: Photo,
    pub registered_photo_file_ids: Vec<FileId>,
    pub file_source_id: FileSourceId,

    pub description: String,

    pub invite_link: DialogInviteLink,

    pub bot_commands: Vec<BotCommands>,

    pub can_set_username: bool,

    pub is_being_updated: bool,
    /// have new changes that need to be sent to the client and database
    pub is_changed: bool,
    /// have new changes that need only to be sent to the client
    pub need_send_update: bool,
    /// have new changes that need only to be saved to the database
    pub need_save_to_database: bool,
    pub is_update_chat_full_sent: bool,
}

impl ChatFull {
    pub fn new() -> ChatFull {
        ChatFull {
            version: -1,
            creator_user_id: UserId::default(),
            participants: Vec::new(),
            photo: Photo::default(),
            registered_photo_file_ids: Vec::new(),
            file_source_id: FileSourceId::default(),
            description: String::new(),
            invite_link: DialogInviteLink::default(),
            bot_commands: Vec::new(),
            can_set_username: false,
            is_being_updated: false,
            is_changed: true,
            need_send_update: true,
            need_save_to_database: true,
            is_update_chat_full_sent: false,
        }
    }

    /// `ChatFull::store` (ChatManager.cpp:1960-1990) with the photo
    /// persistence deferred (the UserFull convention).
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_description = !self.description.is_empty();
        let has_legacy_invite_link = false;
        let has_photo = false; // DIVERGENCE(Files step): !photo.is_empty()
        let has_invite_link = self.invite_link.is_valid();
        let has_bot_commands = !self.bot_commands.is_empty();
        store_flags(
            out,
            [
                has_description,
                has_legacy_invite_link,
                self.can_set_username,
                has_photo,
                has_invite_link,
                has_bot_commands,
            ],
        );
        store_i32(out, self.version);
        store_i64(out, self.creator_user_id.get());
        store_i32(out, self.participants.len() as i32);
        for participant in &self.participants {
            participant.store(out);
        }
        if has_description {
            store_string(out, &self.description);
        }
        // if (has_photo) { store(photo, storer); } — Files step
        if has_invite_link {
            self.invite_link.store(out);
        }
        if has_bot_commands {
            store_i32(out, self.bot_commands.len() as i32);
            for commands in &self.bot_commands {
                commands.store(out);
            }
        }
    }

    /// `ChatFull::parse` (ChatManager.cpp:1992-2027).
    pub fn parse_value(input: &[u8]) -> Result<ChatFull, &'static str> {
        let mut parser = Parser::new_log_event(input)?;
        let chat_full = ChatFull::parse_impl(&mut parser)?;
        parser.fetch_end()?;
        Ok(chat_full)
    }

    pub fn parse_impl(parser: &mut Parser<'_>) -> Result<ChatFull, &'static str> {
        let [
            has_description,
            legacy_has_invite_link,
            can_set_username,
            has_photo,
            has_invite_link,
            has_bot_commands,
        ] = parser.parse_flags()?;
        if has_photo {
            return Err("ChatFull photo needs the FileManager format (Files step)");
        }

        let mut chat_full = ChatFull::new();
        chat_full.can_set_username = can_set_username;
        chat_full.version = parser.i32_()?;
        chat_full.creator_user_id = parse_user_id(parser)?;
        let participant_count = parser.i32_()?;
        if participant_count < 0 {
            return Err("Wrong vector length");
        }
        chat_full.participants = Vec::with_capacity(participant_count.clamp(0, 1_000_000) as usize);
        for _ in 0..participant_count {
            chat_full
                .participants
                .push(DialogParticipant::parse_impl(parser)?);
        }
        if has_description {
            chat_full.description = parser.string()?;
        }
        if legacy_has_invite_link {
            // the C++ parses the legacy link into a local and drops it
            let _legacy_invite_link = parser.string()?;
        }
        // if (has_photo) { parse(photo, parser); } — rejected above
        if has_invite_link {
            chat_full.invite_link = DialogInviteLink::parse_impl(parser)?;
        }
        if has_bot_commands {
            let commands_count = parser.i32_()?;
            if commands_count < 0 {
                return Err("Wrong vector length");
            }
            chat_full.bot_commands =
                Vec::with_capacity(commands_count.clamp(0, 1_000_000) as usize);
            for _ in 0..commands_count {
                chat_full
                    .bot_commands
                    .push(BotCommands::parse_impl(parser)?);
            }
        }
        Ok(chat_full)
    }
}

impl Default for ChatFull {
    fn default() -> Self {
        ChatFull::new()
    }
}

/// `ChatManager::Channel` (ChatManager.h:547-689) — one supergroup or
/// broadcast channel.
#[derive(Clone, Debug, PartialEq)]
pub struct Channel {
    pub access_hash: i64,
    pub title: String,
    pub photo: DialogPhoto,
    pub emoji_status: Option<EmojiStatus>,
    pub last_sent_emoji_status: Option<EmojiStatus>,
    pub accent_color_id: AccentColorId,
    pub background_custom_emoji_id: CustomEmojiId,
    pub profile_accent_color_id: AccentColorId,
    pub profile_background_custom_emoji_id: CustomEmojiId,
    pub usernames: Usernames,
    pub restriction_reasons: Vec<RestrictionReason>,
    pub status: DialogParticipantStatus,
    pub default_permissions: RestrictedRights,
    pub date: i32,
    pub participant_count: i32,
    pub boost_level: i32,
    pub paid_message_star_count: i64,
    pub bot_verification_icon: CustomEmojiId,

    pub max_active_story_id_next_reload_time: f64,
    pub max_active_story_id: StoryId,
    pub max_read_story_id: StoryId,

    pub monoforum_channel_id: ChannelId,
    pub linked_community_id: CommunityId,

    pub cache_version: u32,

    pub has_linked_channel: bool,
    pub has_location: bool,
    pub sign_messages: bool,
    pub show_message_sender: bool,
    pub is_slow_mode_enabled: bool,
    pub noforwards: bool,
    pub can_be_deleted: bool,
    pub join_to_send: bool,
    pub join_request: bool,
    pub stories_hidden: bool,
    pub autotranslation: bool,
    pub broadcast_messages_allowed: bool,
    pub has_live_story: bool,

    pub is_megagroup: bool,
    pub is_gigagroup: bool,
    pub is_forum: bool,
    pub is_forum_tabs: bool,
    pub is_monoforum: bool,
    pub is_admined_monoforum: bool,
    pub is_verified: bool,
    pub is_scam: bool,
    pub is_fake: bool,

    pub is_title_changed: bool,
    pub is_username_changed: bool,
    pub is_photo_changed: bool,
    pub is_emoji_status_changed: bool,
    pub is_accent_color_changed: bool,
    pub is_default_permissions_changed: bool,
    pub is_status_changed: bool,
    pub is_stories_hidden_changed: bool,
    pub is_has_location_changed: bool,
    pub is_noforwards_changed: bool,
    pub is_is_forum_changed: bool,
    pub is_creator_changed: bool,
    pub is_admined_monoforum_changed: bool,
    pub had_read_access: bool,
    pub is_being_updated: bool,
    /// have new changes that need to be sent to the client and database
    pub is_changed: bool,
    /// have new changes that need only to be saved to the database
    pub need_save_to_database: bool,
    pub is_update_supergroup_sent: bool,

    /// whether cached value is rechecked
    pub is_repaired: bool,

    /// is current channel version being saved/is saved to the database
    pub is_saved: bool,
    pub is_being_saved: bool,

    /// true, if the channel was received from the server and not the
    /// database
    pub is_received_from_server: bool,

    pub log_event_id: u64,
}

impl Channel {
    pub const CACHE_VERSION: u32 = 11;

    pub fn new() -> Channel {
        Channel {
            access_hash: 0,
            title: String::new(),
            photo: DialogPhoto::default(),
            emoji_status: None,
            last_sent_emoji_status: None,
            accent_color_id: AccentColorId::default(),
            background_custom_emoji_id: CustomEmojiId::default(),
            profile_accent_color_id: AccentColorId::default(),
            profile_background_custom_emoji_id: CustomEmojiId::default(),
            usernames: Usernames::default(),
            restriction_reasons: Vec::new(),
            status: DialogParticipantStatus::banned(0, String::new()),
            default_permissions: RestrictedRights::restrict_all(),
            date: 0,
            participant_count: 0,
            boost_level: 0,
            paid_message_star_count: 0,
            bot_verification_icon: CustomEmojiId::default(),
            max_active_story_id_next_reload_time: 0.0,
            max_active_story_id: StoryId::default(),
            max_read_story_id: StoryId::default(),
            monoforum_channel_id: ChannelId::default(),
            linked_community_id: CommunityId::default(),
            cache_version: 0,
            has_linked_channel: false,
            has_location: false,
            sign_messages: false,
            show_message_sender: false,
            is_slow_mode_enabled: false,
            noforwards: false,
            can_be_deleted: false,
            join_to_send: false,
            join_request: false,
            stories_hidden: false,
            autotranslation: false,
            broadcast_messages_allowed: false,
            has_live_story: false,
            is_megagroup: false,
            is_gigagroup: false,
            is_forum: false,
            is_forum_tabs: false,
            is_monoforum: false,
            is_admined_monoforum: false,
            is_verified: false,
            is_scam: false,
            is_fake: false,
            is_title_changed: true,
            is_username_changed: true,
            is_photo_changed: true,
            is_emoji_status_changed: true,
            is_accent_color_changed: true,
            is_default_permissions_changed: true,
            is_status_changed: true,
            is_stories_hidden_changed: true,
            is_has_location_changed: true,
            is_noforwards_changed: true,
            is_is_forum_changed: true,
            is_creator_changed: true,
            is_admined_monoforum_changed: false,
            had_read_access: true,
            is_being_updated: false,
            is_changed: true,
            need_save_to_database: true,
            is_update_supergroup_sent: false,
            is_repaired: false,
            is_saved: false,
            is_being_saved: false,
            is_received_from_server: false,
            log_event_id: 0,
        }
    }

    /// `Channel::store` (ChatManager.cpp:2030-2175) with the photo
    /// persistence deferred (the User::store convention).
    pub fn store(&self, out: &mut Vec<u8>, ctx: &TimeCtx) {
        let has_photo = false; // DIVERGENCE(Files step): photo.small_file_id.is_valid()
        let legacy_has_username = false;
        let use_new_rights = true;
        let has_participant_count = self.participant_count != 0;
        let have_default_permissions = true;
        let has_cache_version = self.cache_version != 0;
        let has_restriction_reasons = !self.restriction_reasons.is_empty();
        let legacy_has_active_group_call = false;
        let has_usernames = !self.usernames.is_empty();
        let has_flags2 = true;
        let has_max_active_story_id = self.max_active_story_id.is_valid();
        let has_max_read_story_id = self.max_read_story_id.is_valid();
        let has_max_active_story_id_next_reload_time =
            self.max_active_story_id_next_reload_time > ctx.now;
        let has_accent_color_id = self.accent_color_id.is_valid();
        let has_background_custom_emoji_id = self.background_custom_emoji_id.is_valid();
        let has_profile_accent_color_id = self.profile_accent_color_id.is_valid();
        let has_profile_background_custom_emoji_id =
            self.profile_background_custom_emoji_id.is_valid();
        let has_boost_level = self.boost_level != 0;
        let has_emoji_status = self.emoji_status.is_some();
        let has_bot_verification_icon = self.bot_verification_icon.is_valid();
        let has_paid_message_star_count = self.paid_message_star_count != 0;
        let has_monoforum_channel_id = self.monoforum_channel_id.is_valid();
        let has_linked_community_id = self.linked_community_id.is_valid();
        store_flags(
            out,
            [
                false,
                false,
                false,
                self.sign_messages,
                false,
                false,
                false,
                self.is_megagroup,
                self.is_verified,
                has_photo,
                legacy_has_username,
                false,
                use_new_rights,
                has_participant_count,
                have_default_permissions,
                self.is_scam,
                has_cache_version,
                self.has_linked_channel,
                self.has_location,
                self.is_slow_mode_enabled,
                has_restriction_reasons,
                legacy_has_active_group_call,
                self.is_fake,
                self.is_gigagroup,
                self.noforwards,
                self.can_be_deleted,
                self.join_to_send,
                self.join_request,
                has_usernames,
                has_flags2,
            ],
        );
        if has_flags2 {
            store_flags(
                out,
                [
                    self.is_forum,
                    has_max_active_story_id,
                    has_max_read_story_id,
                    has_max_active_story_id_next_reload_time,
                    self.stories_hidden,
                    has_accent_color_id,
                    has_background_custom_emoji_id,
                    has_profile_accent_color_id,
                    has_profile_background_custom_emoji_id,
                    has_boost_level,
                    has_emoji_status,
                    self.show_message_sender,
                    has_bot_verification_icon,
                    has_paid_message_star_count,
                    self.autotranslation,
                    self.is_monoforum,
                    self.broadcast_messages_allowed,
                    has_monoforum_channel_id,
                    self.is_forum_tabs,
                    self.is_admined_monoforum,
                    self.has_live_story,
                    has_linked_community_id,
                ],
            );
        }

        self.status.store(out);
        store_i64(out, self.access_hash);
        store_string(out, &self.title);
        // if (has_photo) { store(photo, storer); } — Files step
        store_i32(out, self.date);
        if has_restriction_reasons {
            store_restriction_reasons(out, &self.restriction_reasons);
        }
        if has_participant_count {
            store_i32(out, self.participant_count);
        }
        if self.is_megagroup {
            self.default_permissions.store(out);
        }
        if has_cache_version {
            store_u32(out, self.cache_version);
        }
        if has_usernames {
            self.usernames.store(out);
        }
        if has_max_active_story_id {
            store_i32(out, self.max_active_story_id.get());
        }
        if has_max_read_story_id {
            store_i32(out, self.max_read_story_id.get());
        }
        if has_max_active_story_id_next_reload_time {
            TimeCtx::store_time(out, self.max_active_story_id_next_reload_time, ctx);
        }
        if has_accent_color_id {
            store_i32(out, self.accent_color_id.get());
        }
        if has_background_custom_emoji_id {
            store_i64(out, self.background_custom_emoji_id.get());
        }
        if has_profile_accent_color_id {
            store_i32(out, self.profile_accent_color_id.get());
        }
        if has_profile_background_custom_emoji_id {
            store_i64(out, self.profile_background_custom_emoji_id.get());
        }
        if has_boost_level {
            store_i32(out, self.boost_level);
        }
        if has_emoji_status {
            self.emoji_status.as_ref().unwrap().store(out);
        }
        if has_bot_verification_icon {
            store_i64(out, self.bot_verification_icon.get());
        }
        if has_paid_message_star_count {
            store_i64(out, self.paid_message_star_count);
        }
        if has_monoforum_channel_id {
            store_i64(out, self.monoforum_channel_id.get());
        }
        if has_linked_community_id {
            store_i64(out, self.linked_community_id.get());
        }
    }

    /// `log_event_store(Channel)` — the version-prefixed database value.
    pub fn store_value(&self, out: &mut Vec<u8>, ctx: &TimeCtx) {
        crate::store_parse::store_log_event_version(out);
        self.store(out, ctx);
    }

    /// `Channel::parse` (ChatManager.cpp:2178-2384), including the legacy
    /// rights/username/restriction reconstruction and the final status
    /// downgrade for restricted non-megagroups.
    pub fn parse_value(input: &[u8], ctx: &TimeCtx) -> Result<Channel, &'static str> {
        let mut parser = Parser::new_log_event(input)?;
        let channel = Channel::parse_impl(&mut parser, ctx)?;
        parser.fetch_end()?;
        Ok(channel)
    }

    pub fn parse_impl(parser: &mut Parser<'_>, ctx: &TimeCtx) -> Result<Channel, &'static str> {
        let [
            left,
            kicked,
            anyone_can_invite,
            sign_messages,
            is_creator,
            can_edit,
            can_moderate,
            is_megagroup,
            is_verified,
            has_photo,
            legacy_has_username,
            legacy_is_restricted,
            use_new_rights,
            has_participant_count,
            have_default_permissions,
            is_scam,
            has_cache_version,
            has_linked_channel,
            has_location,
            is_slow_mode_enabled,
            has_restriction_reasons,
            legacy_has_active_group_call,
            is_fake,
            is_gigagroup,
            noforwards,
            can_be_deleted,
            join_to_send,
            join_request,
            has_usernames,
            has_flags2,
        ] = parser.parse_flags()?;
        let [
            is_forum,
            has_max_active_story_id,
            has_max_read_story_id,
            has_max_active_story_id_next_reload_time,
            stories_hidden,
            has_accent_color_id,
            has_background_custom_emoji_id,
            has_profile_accent_color_id,
            has_profile_background_custom_emoji_id,
            has_boost_level,
            has_emoji_status,
            show_message_sender,
            has_bot_verification_icon,
            has_paid_message_star_count,
            autotranslation,
            is_monoforum,
            broadcast_messages_allowed,
            has_monoforum_channel_id,
            is_forum_tabs,
            is_admined_monoforum,
            has_live_story,
            has_linked_community_id,
        ] = if has_flags2 {
            parser.parse_flags()?
        } else {
            [false; 22]
        };
        if has_photo {
            return Err("Channel photo needs the FileManager format (Files step)");
        }

        let mut channel = Channel::new();
        channel.sign_messages = sign_messages;
        channel.is_megagroup = is_megagroup;
        channel.is_verified = is_verified;
        channel.is_scam = is_scam;
        channel.has_linked_channel = has_linked_channel;
        channel.has_location = has_location;
        channel.is_slow_mode_enabled = is_slow_mode_enabled;
        channel.is_fake = is_fake;
        channel.is_gigagroup = is_gigagroup;
        channel.noforwards = noforwards;
        channel.can_be_deleted = can_be_deleted;
        channel.join_to_send = join_to_send;
        channel.join_request = join_request;
        channel.is_forum = is_forum;
        channel.stories_hidden = stories_hidden;
        channel.show_message_sender = show_message_sender;
        channel.autotranslation = autotranslation;
        channel.is_monoforum = is_monoforum;
        channel.broadcast_messages_allowed = broadcast_messages_allowed;
        channel.is_forum_tabs = is_forum_tabs;
        channel.is_admined_monoforum = is_admined_monoforum;
        channel.has_live_story = has_live_story;

        if use_new_rights {
            channel.status = DialogParticipantStatus::parse_impl(parser)?;
        } else {
            if kicked {
                channel.status = DialogParticipantStatus::banned(0, String::new());
            } else if left {
                channel.status = DialogParticipantStatus::left();
            } else if is_creator {
                channel.status = DialogParticipantStatus::creator(true, false, String::new());
            } else if can_edit || can_moderate {
                channel.status =
                    DialogParticipantStatus::channel_administrator(false, is_megagroup);
            } else {
                channel.status = DialogParticipantStatus::member(0, String::new());
            }
        }
        channel.access_hash = parser.i64_()?;
        channel.title = parser.string()?;
        // if (has_photo) { parse(photo, parser); } — rejected above
        if legacy_has_username {
            if has_usernames {
                return Err("Have invalid channel flags");
            }
            let username = parser.string()?;
            channel.usernames = Usernames::from_telegram(username, Vec::new());
        }
        channel.date = parser.i32_()?;
        if legacy_is_restricted {
            let restriction_reason = parser.string()?;
            channel.restriction_reasons = get_restriction_reasons_from_legacy(&restriction_reason);
        } else if has_restriction_reasons {
            channel.restriction_reasons = parse_restriction_reasons(parser)?;
        }
        if has_participant_count {
            channel.participant_count = parser.i32_()?;
        }
        if is_megagroup {
            if have_default_permissions {
                channel.default_permissions = RestrictedRights::parse_impl(parser)?;
            } else {
                channel.default_permissions = RestrictedRights::from_bools(
                    true,
                    true,
                    true,
                    true,
                    true,
                    true,
                    true,
                    true,
                    true,
                    true,
                    true,
                    true,
                    true,
                    false,
                    anyone_can_invite,
                    false,
                    false,
                    false,
                    true,
                    true,
                    ChannelType::Megagroup,
                );
            }
        }
        if has_cache_version {
            channel.cache_version = parser.u32_()?;
        }
        if has_usernames {
            channel.usernames = Usernames::parse_tail(parser)?;
        }
        if has_max_active_story_id {
            channel.max_active_story_id = StoryId::new(parser.i32_()?);
        }
        if has_max_read_story_id {
            channel.max_read_story_id = StoryId::new(parser.i32_()?);
        }
        if has_max_active_story_id_next_reload_time {
            channel.max_active_story_id_next_reload_time = TimeCtx::parse_time(parser, ctx)?;
        }
        if has_accent_color_id {
            channel.accent_color_id = AccentColorId::new(parser.i32_()?);
        }
        if has_background_custom_emoji_id {
            channel.background_custom_emoji_id = CustomEmojiId::new(parser.i64_()?);
        }
        if has_profile_accent_color_id {
            channel.profile_accent_color_id = AccentColorId::new(parser.i32_()?);
        }
        if has_profile_background_custom_emoji_id {
            channel.profile_background_custom_emoji_id = CustomEmojiId::new(parser.i64_()?);
        }
        if has_boost_level {
            channel.boost_level = parser.i32_()?;
        }
        if has_emoji_status {
            channel.emoji_status = Some(EmojiStatus::parse_from(parser)?);
        }
        if has_bot_verification_icon {
            channel.bot_verification_icon = CustomEmojiId::new(parser.i64_()?);
        }
        if has_paid_message_star_count {
            channel.paid_message_star_count = parser.i64_()?;
        }
        if has_monoforum_channel_id {
            channel.monoforum_channel_id = parse_channel_id(parser)?;
        }
        if has_linked_community_id {
            channel.linked_community_id = CommunityId::new(parser.i64_()?);
        }

        if !tdutils::utf8::check_utf8(channel.title.as_bytes()) {
            tdutils::log_error!("Have invalid channel title");
            channel.title.clear();
            channel.cache_version = 0;
        }
        if legacy_has_active_group_call {
            channel.cache_version = 0;
        }
        if is_megagroup {
            channel.show_message_sender = true;
        } else if channel.status.is_restricted() {
            if channel.status.is_member() {
                channel.status = DialogParticipantStatus::member(0, String::new());
            } else {
                channel.status = DialogParticipantStatus::left();
            }
        }
        Ok(channel)
    }
}

impl Default for Channel {
    fn default() -> Self {
        Channel::new()
    }
}

/// `ChatManager::ChannelFull` (ChatManager.h:691-812) — one
/// supergroup/broadcast channel's full info.
#[derive(Clone, Debug, PartialEq)]
pub struct ChannelFull {
    pub photo: Photo,
    pub registered_photo_file_ids: Vec<FileId>,
    pub file_source_id: FileSourceId,

    pub description: String,
    pub participant_count: i32,
    pub administrator_count: i32,
    pub restricted_count: i32,
    pub banned_count: i32,
    pub boost_count: i32,
    pub unrestrict_boost_count: i32,
    pub gift_count: i32,

    pub send_paid_message_stars: i64,

    pub invite_link: DialogInviteLink,

    pub bot_commands: Vec<BotCommands>,
    pub bot_verification: Option<BotVerification>,
    pub guard_bot_user_id: UserId,

    pub speculative_version: u32,
    pub repair_request_version: u32,

    pub sticker_set_id: StickerSetId,
    pub emoji_sticker_set_id: StickerSetId,

    pub linked_channel_id: ChannelId,
    pub monoforum_channel_id: ChannelId,
    pub linked_community_id: CommunityId,

    pub location: DialogLocation,

    pub main_profile_tab: ProfileTab,

    pub stats_dc_id: DcId,

    pub slow_mode_delay: i32,
    pub slow_mode_next_send_date: i32,

    pub migrated_from_max_message_id: MessageId,
    pub migrated_from_chat_id: ChatId,

    pub bot_user_ids: Vec<UserId>,

    pub can_get_participants: bool,
    pub has_hidden_participants: bool,
    pub can_set_username: bool,
    pub can_set_sticker_set: bool,
    pub can_set_location: bool,
    pub can_view_statistics: bool,
    pub is_can_view_statistics_inited: bool,
    pub can_view_revenue: bool,
    pub can_view_star_revenue: bool,
    pub is_all_history_available: bool,
    pub can_have_sponsored_messages: bool,
    pub has_aggressive_anti_spam_enabled: bool,
    pub can_be_deleted: bool,
    pub has_pinned_stories: bool,
    /// also used for paid reactions
    pub has_paid_media_allowed: bool,
    pub has_stargifts_available: bool,
    pub has_paid_messages_available: bool,

    pub is_photo_changed: bool,
    pub is_slow_mode_next_send_date_changed: bool,
    pub is_being_updated: bool,
    /// have new changes that need to be sent to the client and database
    pub is_changed: bool,
    /// have new changes that need only to be sent to the client
    pub need_send_update: bool,
    /// have new changes that need only to be saved to the database
    pub need_save_to_database: bool,
    pub is_update_channel_full_sent: bool,

    pub expires_at: f64,
}

impl ChannelFull {
    pub fn new() -> ChannelFull {
        ChannelFull {
            photo: Photo::default(),
            registered_photo_file_ids: Vec::new(),
            file_source_id: FileSourceId::default(),
            description: String::new(),
            participant_count: 0,
            administrator_count: 0,
            restricted_count: 0,
            banned_count: 0,
            boost_count: 0,
            unrestrict_boost_count: 0,
            gift_count: 0,
            send_paid_message_stars: 0,
            invite_link: DialogInviteLink::default(),
            bot_commands: Vec::new(),
            bot_verification: None,
            guard_bot_user_id: UserId::default(),
            speculative_version: 1,
            repair_request_version: 0,
            sticker_set_id: StickerSetId::default(),
            emoji_sticker_set_id: StickerSetId::default(),
            linked_channel_id: ChannelId::default(),
            monoforum_channel_id: ChannelId::default(),
            linked_community_id: CommunityId::default(),
            location: DialogLocation::default(),
            main_profile_tab: ProfileTab::default(),
            stats_dc_id: DcId::default(),
            slow_mode_delay: 0,
            slow_mode_next_send_date: 0,
            migrated_from_max_message_id: MessageId::default(),
            migrated_from_chat_id: ChatId::default(),
            bot_user_ids: Vec::new(),
            can_get_participants: false,
            has_hidden_participants: false,
            can_set_username: false,
            can_set_sticker_set: false,
            can_set_location: false,
            can_view_statistics: false,
            is_can_view_statistics_inited: false,
            can_view_revenue: false,
            can_view_star_revenue: false,
            is_all_history_available: true,
            can_have_sponsored_messages: true,
            has_aggressive_anti_spam_enabled: false,
            can_be_deleted: false,
            has_pinned_stories: false,
            has_paid_media_allowed: false,
            has_stargifts_available: false,
            has_paid_messages_available: false,
            is_photo_changed: true,
            is_slow_mode_next_send_date_changed: true,
            is_being_updated: false,
            is_changed: true,
            need_send_update: true,
            need_save_to_database: true,
            is_update_channel_full_sent: false,
            expires_at: 0.0,
        }
    }

    /// `ChannelFull::store` (ChatManager.cpp:2387-2553) with the photo
    /// persistence deferred (the UserFull convention).
    pub fn store(&self, out: &mut Vec<u8>, ctx: &TimeCtx) {
        let has_description = !self.description.is_empty();
        let has_administrator_count = self.administrator_count != 0;
        let has_restricted_count = self.restricted_count != 0;
        let has_banned_count = self.banned_count != 0;
        let legacy_has_invite_link = false;
        let has_sticker_set = self.sticker_set_id.is_valid();
        let has_linked_channel_id = self.linked_channel_id.is_valid();
        let has_migrated_from_max_message_id = self.migrated_from_max_message_id.is_valid();
        let has_migrated_from_chat_id = self.migrated_from_chat_id.is_valid();
        let has_location = !self.location.empty();
        let has_bot_user_ids = !self.bot_user_ids.is_empty();
        let is_slow_mode_enabled = self.slow_mode_delay != 0;
        let is_slow_mode_delay_active = self.slow_mode_next_send_date != 0;
        let has_stats_dc_id = self.stats_dc_id.is_exact();
        let has_photo = false; // DIVERGENCE(Files step): !photo.is_empty()
        let legacy_has_active_group_call_id = false;
        let has_invite_link = self.invite_link.is_valid();
        let has_bot_commands = !self.bot_commands.is_empty();
        let has_flags2 = true;
        let has_emoji_sticker_set = self.emoji_sticker_set_id.is_valid();
        let has_boost_count = self.boost_count != 0;
        let has_unrestrict_boost_count = self.unrestrict_boost_count != 0;
        let has_can_have_sponsored_messages = true;
        let has_bot_verification = self.bot_verification.is_some();
        let has_gift_count = self.gift_count != 0;
        let has_monoforum_channel_id = self.monoforum_channel_id.is_valid();
        let has_send_paid_message_stars = self.send_paid_message_stars != 0;
        let has_main_profile_tab = self.main_profile_tab != ProfileTab::Default;
        let has_guard_bot_user_id = self.guard_bot_user_id.is_valid();
        let has_linked_community_id = self.linked_community_id.is_valid();
        store_flags(
            out,
            [
                has_description,
                has_administrator_count,
                has_restricted_count,
                has_banned_count,
                legacy_has_invite_link,
                has_sticker_set,
                has_linked_channel_id,
                has_migrated_from_max_message_id,
                has_migrated_from_chat_id,
                self.can_get_participants,
                self.can_set_username,
                self.can_set_sticker_set,
                false, // legacy_can_view_statistics
                self.is_all_history_available,
                self.can_set_location,
                has_location,
                has_bot_user_ids,
                is_slow_mode_enabled,
                is_slow_mode_delay_active,
                has_stats_dc_id,
                has_photo,
                self.is_can_view_statistics_inited,
                self.can_view_statistics,
                legacy_has_active_group_call_id,
                has_invite_link,
                has_bot_commands,
                self.can_be_deleted,
                self.has_aggressive_anti_spam_enabled,
                self.has_hidden_participants,
                has_flags2,
            ],
        );
        if has_flags2 {
            store_flags(
                out,
                [
                    self.has_pinned_stories,
                    has_emoji_sticker_set,
                    has_boost_count,
                    has_unrestrict_boost_count,
                    self.can_have_sponsored_messages,
                    self.can_view_revenue,
                    has_can_have_sponsored_messages,
                    self.has_paid_media_allowed,
                    self.can_view_star_revenue,
                    has_bot_verification,
                    has_gift_count,
                    self.has_stargifts_available,
                    self.has_paid_messages_available,
                    has_monoforum_channel_id,
                    has_send_paid_message_stars,
                    has_main_profile_tab,
                    has_guard_bot_user_id,
                    has_linked_community_id,
                ],
            );
        }
        if has_description {
            store_string(out, &self.description);
        }
        store_i32(out, self.participant_count);
        if has_administrator_count {
            store_i32(out, self.administrator_count);
        }
        if has_restricted_count {
            store_i32(out, self.restricted_count);
        }
        if has_banned_count {
            store_i32(out, self.banned_count);
        }
        if has_sticker_set {
            store_i64(out, self.sticker_set_id.get());
        }
        if has_linked_channel_id {
            store_i64(out, self.linked_channel_id.get());
        }
        if has_location {
            self.location.store(out);
        }
        if has_bot_user_ids {
            store_i32(out, self.bot_user_ids.len() as i32);
            for bot_user_id in &self.bot_user_ids {
                store_i64(out, bot_user_id.get());
            }
        }
        if has_migrated_from_max_message_id {
            self.migrated_from_max_message_id.store(out);
        }
        if has_migrated_from_chat_id {
            store_i64(out, self.migrated_from_chat_id.get());
        }
        if is_slow_mode_enabled {
            store_i32(out, self.slow_mode_delay);
        }
        if is_slow_mode_delay_active {
            store_i32(out, self.slow_mode_next_send_date);
        }
        TimeCtx::store_time(out, self.expires_at, ctx);
        if has_stats_dc_id {
            store_i32(out, self.stats_dc_id.get_raw_id());
        }
        // if (has_photo) { store(photo, storer); } — Files step
        if has_invite_link {
            self.invite_link.store(out);
        }
        if has_bot_commands {
            store_i32(out, self.bot_commands.len() as i32);
            for commands in &self.bot_commands {
                commands.store(out);
            }
        }
        if has_emoji_sticker_set {
            store_i64(out, self.emoji_sticker_set_id.get());
        }
        if has_boost_count {
            store_i32(out, self.boost_count);
        }
        if has_unrestrict_boost_count {
            store_i32(out, self.unrestrict_boost_count);
        }
        if has_bot_verification {
            self.bot_verification.as_ref().unwrap().store(out);
        }
        if has_gift_count {
            store_i32(out, self.gift_count);
        }
        if has_monoforum_channel_id {
            store_i64(out, self.monoforum_channel_id.get());
        }
        if has_send_paid_message_stars {
            store_i64(out, self.send_paid_message_stars);
        }
        if has_main_profile_tab {
            store_i32(out, self.main_profile_tab.as_raw());
        }
        if has_guard_bot_user_id {
            store_i64(out, self.guard_bot_user_id.get());
        }
        if has_linked_community_id {
            store_i64(out, self.linked_community_id.get());
        }
    }

    /// `ChannelFull::parse` (ChatManager.cpp:2556-2741), including the
    /// statistics/sponsored-messages repair tail.
    pub fn parse_value(input: &[u8], ctx: &TimeCtx) -> Result<ChannelFull, &'static str> {
        let mut parser = Parser::new_log_event(input)?;
        let channel_full = ChannelFull::parse_impl(&mut parser, ctx)?;
        parser.fetch_end()?;
        Ok(channel_full)
    }

    pub fn parse_impl(parser: &mut Parser<'_>, ctx: &TimeCtx) -> Result<ChannelFull, &'static str> {
        let [
            has_description,
            has_administrator_count,
            has_restricted_count,
            has_banned_count,
            legacy_has_invite_link,
            has_sticker_set,
            has_linked_channel_id,
            has_migrated_from_max_message_id,
            has_migrated_from_chat_id,
            can_get_participants,
            can_set_username,
            can_set_sticker_set,
            legacy_can_view_statistics,
            is_all_history_available,
            can_set_location,
            has_location,
            has_bot_user_ids,
            is_slow_mode_enabled,
            is_slow_mode_delay_active,
            has_stats_dc_id,
            has_photo,
            is_can_view_statistics_inited,
            can_view_statistics,
            legacy_has_active_group_call_id,
            has_invite_link,
            has_bot_commands,
            can_be_deleted,
            has_aggressive_anti_spam_enabled,
            has_hidden_participants,
            has_flags2,
        ] = parser.parse_flags()?;
        let [
            has_pinned_stories,
            has_emoji_sticker_set,
            has_boost_count,
            has_unrestrict_boost_count,
            can_have_sponsored_messages,
            can_view_revenue,
            has_can_have_sponsored_messages,
            has_paid_media_allowed,
            can_view_star_revenue,
            has_bot_verification,
            has_gift_count,
            has_stargifts_available,
            has_paid_messages_available,
            has_monoforum_channel_id,
            has_send_paid_message_stars,
            has_main_profile_tab,
            has_guard_bot_user_id,
            has_linked_community_id,
        ] = if has_flags2 {
            parser.parse_flags()?
        } else {
            [false; 18]
        };
        if has_photo {
            return Err("ChannelFull photo needs the FileManager format (Files step)");
        }

        let mut channel_full = ChannelFull::new();
        channel_full.can_get_participants = can_get_participants;
        channel_full.can_set_username = can_set_username;
        channel_full.can_set_sticker_set = can_set_sticker_set;
        channel_full.is_all_history_available = is_all_history_available;
        channel_full.can_set_location = can_set_location;
        channel_full.is_can_view_statistics_inited = is_can_view_statistics_inited;
        channel_full.can_view_statistics = can_view_statistics;
        channel_full.can_be_deleted = can_be_deleted;
        channel_full.has_aggressive_anti_spam_enabled = has_aggressive_anti_spam_enabled;
        channel_full.has_hidden_participants = has_hidden_participants;
        channel_full.has_pinned_stories = has_pinned_stories;
        channel_full.can_have_sponsored_messages = can_have_sponsored_messages;
        channel_full.can_view_revenue = can_view_revenue;
        channel_full.has_paid_media_allowed = has_paid_media_allowed;
        channel_full.can_view_star_revenue = can_view_star_revenue;
        channel_full.has_stargifts_available = has_stargifts_available;
        channel_full.has_paid_messages_available = has_paid_messages_available;

        if has_description {
            channel_full.description = parser.string()?;
        }
        channel_full.participant_count = parser.i32_()?;
        if has_administrator_count {
            channel_full.administrator_count = parser.i32_()?;
        }
        if has_restricted_count {
            channel_full.restricted_count = parser.i32_()?;
        }
        if has_banned_count {
            channel_full.banned_count = parser.i32_()?;
        }
        if legacy_has_invite_link {
            // the C++ parses the legacy link into a local and drops it
            let _legacy_invite_link = parser.string()?;
        }
        if has_sticker_set {
            channel_full.sticker_set_id = StickerSetId::new(parser.i64_()?);
        }
        if has_linked_channel_id {
            channel_full.linked_channel_id = parse_channel_id(parser)?;
        }
        if has_location {
            channel_full.location = DialogLocation::parse_impl(parser)?;
        }
        if has_bot_user_ids {
            let bot_user_count = parser.i32_()?;
            if bot_user_count < 0 {
                return Err("Wrong vector length");
            }
            channel_full.bot_user_ids =
                Vec::with_capacity(bot_user_count.clamp(0, 1_000_000) as usize);
            for _ in 0..bot_user_count {
                channel_full.bot_user_ids.push(parse_user_id(parser)?);
            }
        }
        if has_migrated_from_max_message_id {
            channel_full.migrated_from_max_message_id = MessageId::parse_from(parser)?;
        }
        if has_migrated_from_chat_id {
            channel_full.migrated_from_chat_id = parse_chat_id(parser)?;
        }
        if is_slow_mode_enabled {
            channel_full.slow_mode_delay = parser.i32_()?;
        }
        if is_slow_mode_delay_active {
            channel_full.slow_mode_next_send_date = parser.i32_()?;
        }
        channel_full.expires_at = TimeCtx::parse_time(parser, ctx)?;
        if has_stats_dc_id {
            channel_full.stats_dc_id = DcId::create(parser.i32_()?);
        }
        // if (has_photo) { parse(photo, parser); } — rejected above
        if legacy_has_active_group_call_id {
            // the C++ parses the legacy InputGroupCallId (two int64s) into
            // a local and drops it
            let _id = parser.i64_()?;
            let _access_hash = parser.i64_()?;
        }
        if has_invite_link {
            channel_full.invite_link = DialogInviteLink::parse_impl(parser)?;
        }
        if has_bot_commands {
            let commands_count = parser.i32_()?;
            if commands_count < 0 {
                return Err("Wrong vector length");
            }
            channel_full.bot_commands =
                Vec::with_capacity(commands_count.clamp(0, 1_000_000) as usize);
            for _ in 0..commands_count {
                channel_full
                    .bot_commands
                    .push(BotCommands::parse_impl(parser)?);
            }
        }
        if has_emoji_sticker_set {
            channel_full.emoji_sticker_set_id = StickerSetId::new(parser.i64_()?);
        }
        if has_boost_count {
            channel_full.boost_count = parser.i32_()?;
        }
        if has_unrestrict_boost_count {
            channel_full.unrestrict_boost_count = parser.i32_()?;
        }
        if has_bot_verification {
            channel_full.bot_verification = Some(BotVerification::parse_impl(parser)?);
        }
        if has_gift_count {
            channel_full.gift_count = parser.i32_()?;
        }
        if has_monoforum_channel_id {
            channel_full.monoforum_channel_id = parse_channel_id(parser)?;
        }
        if has_send_paid_message_stars {
            channel_full.send_paid_message_stars = parser.i64_()?;
        }
        if has_main_profile_tab {
            channel_full.main_profile_tab = ProfileTab::from_raw(parser.i32_()?);
        }
        if has_guard_bot_user_id {
            channel_full.guard_bot_user_id = parse_user_id(parser)?;
        }
        if has_linked_community_id {
            channel_full.linked_community_id = CommunityId::new(parser.i64_()?);
        }

        if legacy_can_view_statistics {
            // the C++ logs at DEBUG level and ignores the flag
        }
        if !channel_full.is_can_view_statistics_inited {
            channel_full.can_view_statistics = channel_full.stats_dc_id.is_exact();
        }
        if !has_can_have_sponsored_messages {
            channel_full.can_have_sponsored_messages = true;
        }
        Ok(channel_full)
    }
}

impl Default for ChannelFull {
    fn default() -> Self {
        ChannelFull::new()
    }
}

/// `ChatManager::ChatLogEvent` store side (`log_event_store`) — the
/// version framing, the chat id and the record body.
fn store_chat_log_event(out: &mut Vec<u8>, chat_id: ChatId, chat: &Chat) {
    crate::store_parse::store_log_event_version(out);
    store_i64(out, chat_id.get());
    chat.store(out);
}

/// The `ChatLogEvent` parse side (`log_event_parse`).
fn parse_chat_log_event(input: &[u8]) -> Result<(ChatId, Chat), &'static str> {
    let mut parser = Parser::new_log_event(input)?;
    let chat_id = parse_chat_id(&mut parser)?;
    let chat = Chat::parse_impl(&mut parser)?;
    parser.fetch_end()?;
    Ok((chat_id, chat))
}

/// `ChatManager::ChannelLogEvent` store side (`log_event_store`).
fn store_channel_log_event(
    out: &mut Vec<u8>,
    channel_id: ChannelId,
    channel: &Channel,
    ctx: &TimeCtx,
) {
    crate::store_parse::store_log_event_version(out);
    store_i64(out, channel_id.get());
    channel.store(out, ctx);
}

/// The `ChannelLogEvent` parse side (`log_event_parse`).
fn parse_channel_log_event(
    input: &[u8],
    ctx: &TimeCtx,
) -> Result<(ChannelId, Channel), &'static str> {
    let mut parser = Parser::new_log_event(input)?;
    let channel_id = parse_channel_id(&mut parser)?;
    let channel = Channel::parse_impl(&mut parser, ctx)?;
    parser.fetch_end()?;
    Ok((channel_id, channel))
}

/// The `ChatManager` data plane (ChatManager.h:16-999): the id-keyed
/// record caches and the `gr`/`ch` sqlite rows. Like the M.4.5
/// `UserManager`, the C++ async save/load sequencing (the per-id promise
/// queues, the `is_being_saved` window) collapses onto the synchronous
/// kv, and the binlog mirror (`ChatLogEvent`/`ChannelLogEvent`) rides the
/// same add/rewrite-while-unsaved, retire-on-save pattern as the user
/// records — `log_event_id`
/// stays 0 and every mirror branch is inert.
pub struct ChatManager {
    chats: BTreeMap<i64, Chat>,
    /// the `grf<id>` plane — M.5.15.
    chats_full: BTreeMap<i64, ChatFull>,
    channels: BTreeMap<i64, Channel>,
    /// the `chf<id>` plane — M.5.16.
    channels_full: BTreeMap<i64, ChannelFull>,
    /// feed the unknown-basic-group/supergroup builders — M.5.36/M.5.37.
    unknown_chats: BTreeSet<i64>,
    unknown_channels: BTreeSet<i64>,
    loaded_from_database_chats: BTreeSet<i64>,
    loaded_from_database_channels: BTreeSet<i64>,
    /// `min_channels_` (ChatManager.h:957) — the degraded channel records
    /// (M.5.21 fills them; the id-object builders already consult them).
    min_channels: BTreeMap<i64, MinChannel>,
    /// `channel_emoji_status_timeout_` / `channel_unban_timeout_` /
    /// `slow_mode_delay_timeout_` (ChatManager.h:994-996) — the member
    /// MultiTimeouts as deadline maps keyed by the raw channel id, driven
    /// by the Td tick (the M.4.7 pattern).
    channel_emoji_status_timeouts: BTreeMap<i64, f64>,
    channel_unban_timeouts: BTreeMap<i64, f64>,
    slow_mode_delay_timeouts: BTreeMap<i64, f64>,
    /// `Td::send_update` fan-out — queued and flushed by the Td actor like
    /// the UserManager's `updateUser` queue.
    pending_updates: Vec<td_api::Update>,
    /// `invalidated_channels_full_` (ChatManager.h:961) — fulls invalidated
    /// before they were ever loaded.
    invalidated_channels_full: BTreeSet<i64>,
    /// `unavailable_chat_fulls_` / `unavailable_channel_fulls_`
    /// (ChatManager.h:976/:980) — fulls known to be absent from the
    /// database (the one-shot load gate).
    unavailable_chat_fulls: BTreeSet<i64>,
    unavailable_channel_fulls: BTreeSet<i64>,
    /// `chat_full_file_source_ids_` / `channel_full_file_source_ids_`
    /// (ChatManager.h:955/:962) — file sources for fulls that were never
    /// materialized in memory.
    chat_full_file_source_ids: BTreeMap<i64, FileSourceId>,
    channel_full_file_source_ids: BTreeMap<i64, FileSourceId>,
    /// `channel_messages_` (ChatManager.h:987) — empty until
    /// register/unregister_message_channels lands (M.5.43); the
    /// input-channel builders consult it.
    channel_messages: BTreeMap<i64, BTreeSet<i64>>,
    /// `linked_channel_ids_` (ChatManager.h:989) — empty until
    /// on_update_channel_full_linked_channel_id lands (M.5.26).
    linked_channel_ids: BTreeMap<i64, i64>,
    /// `monoforum_channel_ids_` (ChatManager.h:990) — in-memory only.
    monoforum_channel_ids: BTreeMap<i64, i64>,
    /// `dialogs_for_discussion_` (ChatManager.h:971) — in-memory only in
    /// the C++ too (re-fetched from the server); fed by
    /// [`ChatManager::update_dialogs_for_discussion`].
    dialogs_for_discussion: Vec<DialogId>,
    dialogs_for_discussion_inited: bool,
    /// `created_public_channels_` / `created_public_channels_inited_`
    /// (ChatManager.h:963-967), indexed by `PublicDialogType` — the pmc
    /// keys `public_channels0/1/2` (ChatManager.cpp:4236-4245).
    created_public_channels: [Vec<i64>; 3],
    created_public_channels_inited: [bool; 3],
    /// `inactive_channel_ids_` (ChatManager.h:977) — in-memory only.
    inactive_channel_ids: Vec<i64>,
    inactive_channel_ids_inited: bool,
    /// `restricted_channel_ids_` (ChatManager.h:992) — the supergroups a
    /// `get_current_state` refire touches when the ignored-restriction
    /// reasons option changes.
    restricted_channel_ids: BTreeSet<i64>,
    /// The `FileReferenceManager::create_*_file_source` stand-in — a local
    /// counter (the Files-step convention), replaced by the real registry
    /// with the Files slice.
    file_source_id_counter: i32,
}

impl ChatManager {
    pub fn new() -> ChatManager {
        ChatManager {
            chats: BTreeMap::new(),
            chats_full: BTreeMap::new(),
            channels: BTreeMap::new(),
            channels_full: BTreeMap::new(),
            unknown_chats: BTreeSet::new(),
            unknown_channels: BTreeSet::new(),
            loaded_from_database_chats: BTreeSet::new(),
            loaded_from_database_channels: BTreeSet::new(),
            min_channels: BTreeMap::new(),
            channel_emoji_status_timeouts: BTreeMap::new(),
            channel_unban_timeouts: BTreeMap::new(),
            slow_mode_delay_timeouts: BTreeMap::new(),
            pending_updates: Vec::new(),
            invalidated_channels_full: BTreeSet::new(),
            unavailable_chat_fulls: BTreeSet::new(),
            unavailable_channel_fulls: BTreeSet::new(),
            chat_full_file_source_ids: BTreeMap::new(),
            channel_full_file_source_ids: BTreeMap::new(),
            channel_messages: BTreeMap::new(),
            linked_channel_ids: BTreeMap::new(),
            monoforum_channel_ids: BTreeMap::new(),
            dialogs_for_discussion: Vec::new(),
            dialogs_for_discussion_inited: false,
            created_public_channels: [Vec::new(), Vec::new(), Vec::new()],
            created_public_channels_inited: [false; 3],
            inactive_channel_ids: Vec::new(),
            inactive_channel_ids_inited: false,
            restricted_channel_ids: BTreeSet::new(),
            file_source_id_counter: 0,
        }
    }

    /// `ChatManager::get_chat_id` (ChatManager.cpp:4409-4423).
    pub fn get_chat_id(chat: &telegram_api::Chat) -> ChatId {
        match chat {
            telegram_api::Chat::chatEmpty(c) => ChatId::new(c.id),
            telegram_api::Chat::chat(c) => ChatId::new(c.id),
            telegram_api::Chat::chatForbidden(c) => ChatId::new(c.id),
            _ => ChatId::default(),
        }
    }

    /// `ChatManager::get_channel_id` (ChatManager.cpp:4425-4436).
    pub fn get_channel_id(chat: &telegram_api::Chat) -> ChannelId {
        match chat {
            telegram_api::Chat::channel(c) => ChannelId::new(c.id),
            telegram_api::Chat::channelForbidden(c) => ChannelId::new(c.id),
            _ => ChannelId::default(),
        }
    }

    /// `ChatManager::get_dialog_id` (ChatManager.cpp:4438-4445).
    pub fn get_dialog_id(chat: &telegram_api::Chat) -> DialogId {
        let channel_id = Self::get_channel_id(chat);
        if channel_id.is_valid() {
            return DialogId::from_channel_id(channel_id);
        }
        DialogId::from_chat_id(Self::get_chat_id(chat))
    }

    /// `have_chat` (ChatManager.cpp:8376).
    pub fn have_chat(&self, chat_id: ChatId) -> bool {
        chat_id.is_valid() && self.chats.contains_key(&chat_id.get())
    }

    /// `get_chat` (ChatManager.cpp:8380-8386).
    pub fn get_chat(&self, chat_id: ChatId) -> Option<&Chat> {
        self.chats.get(&chat_id.get())
    }

    pub fn get_chat_mut(&mut self, chat_id: ChatId) -> Option<&mut Chat> {
        self.chats.get_mut(&chat_id.get())
    }

    /// `add_chat` (ChatManager.cpp:8388-8395) — its production callers are
    /// the M.5.20+ intakes; the tests drive it directly.
    #[allow(dead_code)]
    fn add_chat(&mut self, chat_id: ChatId) -> &mut Chat {
        debug_assert!(chat_id.is_valid());
        self.chats.entry(chat_id.get()).or_default()
    }

    /// `have_channel` (ChatManager.cpp:8860).
    pub fn have_channel(&self, channel_id: ChannelId) -> bool {
        channel_id.is_valid() && self.channels.contains_key(&channel_id.get())
    }

    /// `get_channel` (ChatManager.cpp:8879-8885).
    pub fn get_channel(&self, channel_id: ChannelId) -> Option<&Channel> {
        self.channels.get(&channel_id.get())
    }

    pub fn get_channel_mut(&mut self, channel_id: ChannelId) -> Option<&mut Channel> {
        self.channels.get_mut(&channel_id.get())
    }

    /// `add_channel` (ChatManager.cpp:8887-8895) — see [`ChatManager::add_chat`].
    #[allow(dead_code)]
    fn add_channel(&mut self, channel_id: ChannelId) -> &mut Channel {
        debug_assert!(channel_id.is_valid());
        self.channels.entry(channel_id.get()).or_default()
    }

    /// `get_chat_database_key` (ChatManager.cpp:4508-4510).
    fn get_chat_database_key(chat_id: ChatId) -> Vec<u8> {
        format!("gr{}", chat_id.get()).into_bytes()
    }

    /// `get_chat_database_value` (ChatManager.cpp:4512-4514) — the TL
    /// framing of the record alone (the `ChatLogEvent` adds the id, the
    /// row does not).
    fn get_chat_database_value(chat: &Chat) -> Vec<u8> {
        let mut value = Vec::new();
        chat.store_value(&mut value);
        value
    }

    /// `get_channel_database_key` (ChatManager.cpp:4757-4759).
    fn get_channel_database_key(channel_id: ChannelId) -> Vec<u8> {
        format!("ch{}", channel_id.get()).into_bytes()
    }

    /// `get_channel_database_value` (ChatManager.cpp:4761-4763).
    fn get_channel_database_value(channel: &Channel, ctx: &TimeCtx) -> Vec<u8> {
        let mut value = Vec::new();
        channel.store_value(&mut value, ctx);
        value
    }

    /// `ChatManager::update_chat` (ChatManager.cpp:5246-5345). The
    /// MessagesManager dialog fan-outs (`on_dialog_photo_updated`,
    /// `on_dialog_title_updated`, `on_dialog_default_permissions_updated`,
    /// `on_dialog_access_updated`, `on_dialog_has_protected_content_updated`,
    /// `try_update_dialog_pos`, `drop_dialog_pending_join_requests`) are
    /// the documented M.10 stubs; `update_dialogs_for_discussion` keeps
    /// its C++ shape. The 1.0-second SleepActor hop around the
    /// status-repair reload defers only the net query, so the interim
    /// `reload_chat` call is direct.
    fn update_chat(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        chat_id: ChatId,
        from_binlog: bool,
        from_database: bool,
    ) {
        if self.get_chat(chat_id).is_some_and(|c| c.is_being_updated) {
            tdutils::log_error!("Detected recursive update of {chat_id:?}");
        }
        if let Some(c) = self.get_chat_mut(chat_id) {
            c.is_being_updated = true;
        }

        let mut need_update_chat_full = false;
        let photo_changed = self.get_chat(chat_id).is_some_and(|c| c.is_photo_changed);
        if photo_changed {
            // td_->messages_manager_->on_dialog_photo_updated — the M.10 stub.
            if let Some(c) = self.get_chat_mut(chat_id) {
                c.is_photo_changed = false;
            }
            let chat_full_differs = match (self.get_chat_full(chat_id), self.get_chat(chat_id)) {
                (Some(full), Some(c)) => !is_same_dialog_photo(&full.photo, &c.photo),
                _ => false,
            };
            if chat_full_differs {
                self.on_update_chat_full_photo(env, chat_id, Photo::default());
                if self
                    .get_chat_full(chat_id)
                    .is_some_and(|f| f.is_update_chat_full_sent)
                {
                    need_update_chat_full = true;
                }
                let small_file_id_valid = self
                    .get_chat(chat_id)
                    .is_some_and(|c| c.photo.small_file_id.is_valid());
                if small_file_id_valid {
                    self.reload_chat_full(env, chat_id, Promise::default(), "update_chat");
                }
            }
        }
        let title_changed = self.get_chat(chat_id).is_some_and(|c| c.is_title_changed);
        if title_changed {
            // td_->messages_manager_->on_dialog_title_updated — the M.10 stub.
            if let Some(c) = self.get_chat_mut(chat_id) {
                c.is_title_changed = false;
            }
        }
        let default_permissions_changed = self
            .get_chat(chat_id)
            .is_some_and(|c| c.is_default_permissions_changed);
        if default_permissions_changed {
            // td_->messages_manager_->on_dialog_default_permissions_updated
            // — the M.10 stub.
            if let Some(c) = self.get_chat_mut(chat_id) {
                c.is_default_permissions_changed = false;
            }
        }
        let is_active_changed = self
            .get_chat(chat_id)
            .is_some_and(|c| c.is_is_active_changed);
        if is_active_changed {
            let suitable = self
                .get_chat(chat_id)
                .is_some_and(Self::is_suitable_discussion_chat);
            self.update_dialogs_for_discussion(DialogId::from_chat_id(chat_id), suitable);
            if let Some(c) = self.get_chat_mut(chat_id) {
                c.is_is_active_changed = false;
            }
            // td_->messages_manager_->on_dialog_access_updated — the M.10 stub.
        }
        let status_changed = self.get_chat(chat_id).is_some_and(|c| c.is_status_changed);
        if status_changed {
            // td_->messages_manager_->drop_dialog_pending_join_requests —
            // the M.10 stub.
            if !from_database {
                // td_->messages_manager_->try_update_dialog_pos — the M.10
                // stub. If the chat was already sent to the client, the C++
                // reloads it a second later to repair its status; the
                // SleepActor defers only the net query.
                if self
                    .get_chat(chat_id)
                    .is_some_and(|c| c.is_update_basic_group_sent)
                {
                    self.reload_chat(env, chat_id, Promise::default(), "ReloadChatSleepActor");
                }
            }
            // td_->messages_manager_->on_dialog_access_updated — the M.10 stub.
            if let Some(c) = self.get_chat_mut(chat_id) {
                c.is_status_changed = false;
            }
        }
        let noforwards_changed = self
            .get_chat(chat_id)
            .is_some_and(|c| c.is_noforwards_changed);
        if noforwards_changed {
            // td_->messages_manager_->on_dialog_has_protected_content_updated
            // — the M.10 stub.
            if let Some(c) = self.get_chat_mut(chat_id) {
                c.is_noforwards_changed = false;
            }
        }

        if need_update_chat_full {
            debug_assert!(self.get_chat_full(chat_id).is_some());
            self.update_chat_full(user_manager, env, chat_id, "update_chat", false);
        }

        let changed = {
            let Some(c) = self.get_chat_mut(chat_id) else {
                return;
            };
            tdutils::log_debug!(
                "Update {chat_id:?}: need_save_to_database = {}, is_changed = {}",
                c.need_save_to_database,
                c.is_changed
            );
            c.need_save_to_database |= c.is_changed;
            if c.need_save_to_database {
                if !from_database {
                    c.is_saved = false;
                }
                c.need_save_to_database = false;
            }
            let changed = c.is_changed;
            if changed {
                c.is_changed = false;
                c.is_update_basic_group_sent = true;
            }
            changed
        };
        if changed {
            let c_snapshot = self.get_chat(chat_id).cloned();
            let update = self.get_update_basic_group_object(chat_id, c_snapshot.as_ref());
            self.pending_updates.push(update);
        }
        if !from_database {
            self.save_chat(env, chat_id, from_binlog);
        }
        if let Some(c) = self.get_chat_mut(chat_id) {
            c.is_being_updated = false;
        }
        let need_repair = self
            .get_chat(chat_id)
            .is_some_and(|c| c.cache_version != Chat::CACHE_VERSION && !c.is_repaired)
            && self.have_input_peer_chat(chat_id, AccessRights::Read)
            && !env.global.close_flag();
        if need_repair {
            if let Some(c) = self.get_chat_mut(chat_id) {
                c.is_repaired = true;
            }
            tdutils::log_info!("Repairing cache of {chat_id:?}");
            self.reload_chat(env, chat_id, Promise::default(), "update_chat");
        }
    }

    /// `is_suitable_discussion_chat` (ChatManager.cpp:4088-4090).
    fn is_suitable_discussion_chat(c: &Chat) -> bool {
        c.is_active && c.status.is_creator()
    }

    /// `update_dialogs_for_discussion` (ChatManager.cpp:4319-4332). The
    /// newest-suitable-first insert order matches the C++ (`begin()`).
    fn update_dialogs_for_discussion(&mut self, dialog_id: DialogId, is_suitable: bool) {
        if !self.dialogs_for_discussion_inited {
            return;
        }
        if is_suitable {
            if !self.dialogs_for_discussion.contains(&dialog_id) {
                tdutils::log_debug!("Add {dialog_id:?} to list of suitable discussion chats");
                self.dialogs_for_discussion.insert(0, dialog_id);
            }
        } else if let Some(position) = self
            .dialogs_for_discussion
            .iter()
            .position(|d| *d == dialog_id)
        {
            self.dialogs_for_discussion.remove(position);
            tdutils::log_debug!("Remove {dialog_id:?} from list of suitable discussion chats");
        }
    }

    // ---- M.5.25/M.5.26 helpers: the linked/monoforum/community/location/
    // slow-mode/bot full mutators (ChatManager.cpp:6742-6980, 8214-8245,
    // 9129-9152) ----

    /// `get_linked_channel_id` (ChatManager.cpp:8664-8670) — the full is
    /// consulted first.
    fn get_linked_channel_id(&self, channel_id: ChannelId) -> ChannelId {
        if let Some(f) = self.get_channel_full_const(channel_id) {
            return f.linked_channel_id;
        }
        ChannelId::new(
            self.linked_channel_ids
                .get(&channel_id.get())
                .copied()
                .unwrap_or_default(),
        )
    }

    /// `remove_linked_channel_id` (ChatManager.cpp:6742-6749).
    fn remove_linked_channel_id(&mut self, channel_id: ChannelId) {
        if let Some(linked) = self.linked_channel_ids.remove(&channel_id.get()) {
            self.linked_channel_ids.remove(&linked);
        }
    }

    /// `get_monoforum_channel_id` (ChatManager.cpp:8664+) — the channel
    /// record is consulted first.
    fn get_monoforum_channel_id(&self, channel_id: ChannelId) -> ChannelId {
        if let Some(c) = self.get_channel(channel_id) {
            return c.monoforum_channel_id;
        }
        ChannelId::new(
            self.monoforum_channel_ids
                .get(&channel_id.get())
                .copied()
                .unwrap_or_default(),
        )
    }

    /// `on_update_channel_full_linked_channel_id`
    /// (ChatManager.cpp:6763-6852) — the mutual-pair rewrite. The
    /// `messages_manager on_dialog_linked_channel_updated` fan-outs are
    /// the M.6 stubs.
    fn on_update_channel_full_linked_channel_id(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        channel_id: ChannelId,
        linked_channel_id: ChannelId,
    ) {
        let old_linked_channel_id = self.get_linked_channel_id(channel_id);
        tdutils::log_info!(
            "Update linked channel in {channel_id:?} from {old_linked_channel_id:?} to {linked_channel_id:?}"
        );
        let f_linked_old = self
            .get_channel_full_const(channel_id)
            .map(|f| f.linked_channel_id)
            .unwrap_or_default();
        let changed = f_linked_old != linked_channel_id;

        // warm-ups for the removed pair
        if changed && f_linked_old.is_valid() {
            self.get_channel_force(
                user_manager,
                env,
                f_linked_old,
                "on_update_channel_full_linked_channel_id 10",
                false,
            );
            self.get_channel_full_force(
                user_manager,
                env,
                f_linked_old,
                true,
                "on_update_channel_full_linked_channel_id 0",
                false,
            );
        }
        let old_linked_linked_channel_id = self.get_linked_channel_id(linked_channel_id);
        self.remove_linked_channel_id(channel_id);
        self.remove_linked_channel_id(linked_channel_id);
        if linked_channel_id.is_valid() {
            self.linked_channel_ids
                .insert(channel_id.get(), linked_channel_id.get());
            self.linked_channel_ids
                .insert(linked_channel_id.get(), channel_id.get());
        }

        if changed {
            if f_linked_old.is_valid() {
                // removing the old pair
                let old = f_linked_old;
                self.get_channel_force(
                    user_manager,
                    env,
                    old,
                    "on_update_channel_full_linked_channel_id 11",
                    false,
                );
                let had_link = self.get_channel(old).is_some_and(|c| c.has_linked_channel);
                if had_link {
                    if let Some(c_old) = self.get_channel_mut(old) {
                        c_old.has_linked_channel = false;
                        c_old.is_changed = true;
                    }
                    self.update_channel(user_manager, env, old, false, false);
                    self.reload_channel(
                        env,
                        old,
                        Promise::default(),
                        "on_update_channel_full_linked_channel_id 21",
                    );
                }
                if let Some(f_old) = self.get_channel_full_force(
                    user_manager,
                    env,
                    old,
                    true,
                    "on_update_channel_full_linked_channel_id 1",
                    false,
                ) && f_old.linked_channel_id == channel_id
                {
                    f_old.linked_channel_id = ChannelId::default();
                    f_old.is_changed = true;
                    self.update_channel_full(
                        user_manager,
                        env,
                        old,
                        "on_update_channel_full_linked_channel_id 3",
                        false,
                    );
                }
            }
            if let Some(f) = self.get_channel_full_mut(channel_id) {
                f.linked_channel_id = linked_channel_id;
                f.is_changed = true;
            }
            if linked_channel_id.is_valid() {
                // adding the new pair
                self.get_channel_force(
                    user_manager,
                    env,
                    linked_channel_id,
                    "on_update_channel_full_linked_channel_id 12",
                    false,
                );
                let has_link = self
                    .get_channel(linked_channel_id)
                    .is_some_and(|c| c.has_linked_channel);
                if !has_link {
                    if let Some(c_new) = self.get_channel_mut(linked_channel_id) {
                        c_new.has_linked_channel = true;
                        c_new.is_changed = true;
                    }
                    self.update_channel(user_manager, env, linked_channel_id, false, false);
                    self.reload_channel(
                        env,
                        linked_channel_id,
                        Promise::default(),
                        "on_update_channel_full_linked_channel_id 22",
                    );
                }
                if let Some(f_new) = self.get_channel_full_force(
                    user_manager,
                    env,
                    linked_channel_id,
                    true,
                    "on_update_channel_full_linked_channel_id 2",
                    false,
                ) && f_new.linked_channel_id != channel_id
                {
                    f_new.linked_channel_id = channel_id;
                    f_new.is_changed = true;
                    self.update_channel_full(
                        user_manager,
                        env,
                        linked_channel_id,
                        "on_update_channel_full_linked_channel_id 4",
                        false,
                    );
                }
            }
        }

        debug_assert!(
            self.get_channel(channel_id).is_some(),
            "CHECK(c != nullptr)"
        );
        let has_linked_matches = self
            .get_channel(channel_id)
            .is_some_and(|c| linked_channel_id.is_valid() == c.has_linked_channel);
        if !has_linked_matches {
            if let Some(c) = self.get_channel_mut(channel_id) {
                c.has_linked_channel = linked_channel_id.is_valid();
                c.is_changed = true;
            }
            self.update_channel(user_manager, env, channel_id, false, false);
        }
        if old_linked_channel_id != linked_channel_id {
            // td_->messages_manager_->on_dialog_linked_channel_updated —
            // the M.6 stub; must run after the linked channel changed.
        }
        if linked_channel_id.is_valid() {
            let new_linked_linked = self.get_linked_channel_id(linked_channel_id);
            if old_linked_linked_channel_id != new_linked_linked {
                // td_->messages_manager_->on_dialog_linked_channel_updated
                // for DialogId(linked_channel_id) — the M.6 stub.
            }
        }
    }

    /// `on_update_channel_full_monoforum_channel_id`
    /// (ChatManager.cpp:6854-6897).
    fn on_update_channel_full_monoforum_channel_id(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        channel_id: ChannelId,
        monoforum_channel_id: ChannelId,
    ) {
        let old = self.get_monoforum_channel_id(channel_id);
        tdutils::log_info!(
            "Update monoforum channel in {channel_id:?} from {old:?} to {monoforum_channel_id:?}"
        );
        if old != monoforum_channel_id && monoforum_channel_id.is_valid() && old.is_valid() {
            tdutils::log_error!(
                "Update monoforum channel in {channel_id:?} from {old:?} to {monoforum_channel_id:?}"
            );
        }
        if let Some(f) = self.get_channel_full_mut(channel_id)
            && f.monoforum_channel_id != monoforum_channel_id
            && f.monoforum_channel_id.is_valid()
        {
            // snapshot then release the full borrow before the force-loads
            let old_full = f.monoforum_channel_id;
            self.get_channel_force(
                user_manager,
                env,
                old_full,
                "on_update_channel_full_monoforum_channel_id 10",
                false,
            );
            self.get_channel_full_force(
                user_manager,
                env,
                old_full,
                true,
                "on_update_channel_full_monoforum_channel_id 0",
                false,
            );
        }
        if let Some(f) = self.get_channel_full_mut(channel_id)
            && f.monoforum_channel_id != monoforum_channel_id
        {
            f.monoforum_channel_id = monoforum_channel_id;
            f.is_changed = true;
        }
        debug_assert!(
            self.get_channel(channel_id).is_some(),
            "CHECK(c != nullptr)"
        );
        let (is_monoforum, is_megagroup, broadcast_allowed) = {
            let Some(c) = self.get_channel(channel_id) else {
                return;
            };
            (c.is_monoforum, c.is_megagroup, c.broadcast_messages_allowed)
        };
        if monoforum_channel_id.is_valid() {
            self.monoforum_channel_ids
                .insert(channel_id.get(), monoforum_channel_id.get());
        }
        if !is_monoforum {
            self.monoforum_channel_ids
                .insert(monoforum_channel_id.get(), channel_id.get());
        } else if monoforum_channel_id.is_valid() {
            self.monoforum_channel_ids.remove(&channel_id.get());
        }
        let broadcast_flip = if is_megagroup {
            if monoforum_channel_id.is_valid() != is_monoforum {
                tdutils::log_error!("Receive monoforum {monoforum_channel_id:?} in {channel_id:?}");
            }
            false
        } else {
            monoforum_channel_id.is_valid() != broadcast_allowed
        };
        if broadcast_flip {
            if let Some(c) = self.get_channel_mut(channel_id) {
                c.broadcast_messages_allowed = monoforum_channel_id.is_valid();
                c.is_changed = true;
            }
            self.update_channel(user_manager, env, channel_id, false, false);
        }
    }

    /// `on_update_channel_full_linked_community_id`
    /// (ChatManager.cpp:6899-6913).
    fn on_update_channel_full_linked_community_id(
        &mut self,
        channel_id: ChannelId,
        linked_community_id: CommunityId,
    ) {
        debug_assert!(self.channels_full.contains_key(&channel_id.get()));
        let linked_community_id = if !linked_community_id.is_valid() {
            CommunityId::default()
        } else {
            linked_community_id
        };
        let Some(f) = self.get_channel_full_mut(channel_id) else {
            return;
        };
        if f.linked_community_id == linked_community_id {
            return;
        }
        tdutils::log_info!(
            "Update community in {channel_id:?} from {:?} to {linked_community_id:?}",
            f.linked_community_id
        );
        f.linked_community_id = linked_community_id;
        f.is_changed = true;
    }

    /// `on_update_channel_full_location` (ChatManager.cpp:6915-6926).
    fn on_update_channel_full_location(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        channel_id: ChannelId,
        location: DialogLocation,
    ) {
        if let Some(f) = self.get_channel_full_mut(channel_id)
            && f.location != location
        {
            f.location = location.clone();
            f.is_changed = true;
        }
        debug_assert!(
            self.get_channel(channel_id).is_some(),
            "CHECK(c != nullptr)"
        );
        let has_location = self
            .get_channel(channel_id)
            .is_some_and(|_| !location.empty());
        {
            let Some(c) = self.get_channel_mut(channel_id) else {
                return;
            };
            if c.has_location != has_location {
                tdutils::log_info!(
                    "Update {channel_id:?} has_location from {} to {has_location}",
                    c.has_location
                );
                c.has_location = has_location;
                c.is_has_location_changed = true;
                c.is_changed = true;
            }
        }
        self.update_channel(user_manager, env, channel_id, false, false);
    }

    /// `on_update_channel_full_slow_mode_delay`
    /// (ChatManager.cpp:6928-6949).
    fn on_update_channel_full_slow_mode_delay(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        channel_id: ChannelId,
        mut slow_mode_delay: i32,
        slow_mode_next_send_date: i32,
    ) {
        if slow_mode_delay < 0 {
            tdutils::log_error!("Receive slow mode delay {slow_mode_delay} in {channel_id:?}");
            slow_mode_delay = 0;
        }
        if let Some(f) = self.get_channel_full_mut(channel_id)
            && f.slow_mode_delay != slow_mode_delay
        {
            f.slow_mode_delay = slow_mode_delay;
            f.is_changed = true;
        }
        self.on_update_channel_full_slow_mode_next_send_date(
            env,
            channel_id,
            slow_mode_next_send_date,
        );
        debug_assert!(
            self.get_channel(channel_id).is_some(),
            "CHECK(c != nullptr)"
        );
        let is_slow_mode_enabled = slow_mode_delay != 0;
        {
            let Some(c) = self.get_channel_mut(channel_id) else {
                return;
            };
            if c.is_slow_mode_enabled != is_slow_mode_enabled {
                c.is_slow_mode_enabled = is_slow_mode_enabled;
                c.is_changed = true;
            }
        }
        self.update_channel(user_manager, env, channel_id, false, false);
    }

    /// `on_update_channel_full_slow_mode_next_send_date` — the private
    /// `ChannelFull *` variant (ChatManager.cpp:6951-6980); the public
    /// `on_update_channel_slow_mode_next_send_date` is C++ 8243-8253.
    fn on_update_channel_full_slow_mode_next_send_date(
        &mut self,
        env: &mut UserEnv,
        channel_id: ChannelId,
        mut slow_mode_next_send_date: i32,
    ) {
        if slow_mode_next_send_date < 0 {
            tdutils::log_error!("Receive slow mode next send date {slow_mode_next_send_date}");
            slow_mode_next_send_date = 0;
        }
        let Some(f) = self.channels_full.get_mut(&channel_id.get()) else {
            return;
        };
        if f.slow_mode_delay == 0 && slow_mode_next_send_date > 0 {
            tdutils::log_error!(
                "Slow mode is disabled, but next send date is {slow_mode_next_send_date}"
            );
            slow_mode_next_send_date = 0;
        }
        if slow_mode_next_send_date != 0 {
            let now = env.global.unix_time();
            if slow_mode_next_send_date <= now {
                slow_mode_next_send_date = 0;
            }
            if slow_mode_next_send_date > now + 3601 {
                slow_mode_next_send_date = now + 3601;
            }
        }
        if f.slow_mode_next_send_date != slow_mode_next_send_date {
            f.slow_mode_next_send_date = slow_mode_next_send_date;
            f.is_slow_mode_next_send_date_changed = true;
            if f.unrestrict_boost_count == 0 || f.boost_count < f.unrestrict_boost_count {
                f.is_changed = true;
            } else {
                f.need_save_to_database = true;
            }
        }
    }

    /// `on_update_channel_full_bot_user_ids` (ChatManager.cpp:8214-8222).
    /// The save-only flag — this diff never emits an update by itself.
    fn on_update_channel_full_bot_user_ids(
        &mut self,
        channel_id: ChannelId,
        bot_user_ids: Vec<UserId>,
    ) {
        debug_assert!(self.channels_full.contains_key(&channel_id.get()));
        // td_->messages_manager_->on_dialog_bots_updated — the M.6 stub
        // (send_closure_later).
        let Some(f) = self.get_channel_full_mut(channel_id) else {
            return;
        };
        if f.bot_user_ids != bot_user_ids {
            f.bot_user_ids = bot_user_ids;
            f.need_save_to_database = true;
        }
    }

    /// `on_update_channel_full_guard_bot_user_id`
    /// (ChatManager.cpp:9129-9142).
    fn on_update_channel_full_guard_bot_user_id(
        &mut self,
        user_manager: &mut UserManager,
        channel_id: ChannelId,
        guard_bot_user_id: UserId,
    ) {
        let mut guard_bot_user_id = guard_bot_user_id;
        if guard_bot_user_id != UserId::default() && !guard_bot_user_id.is_valid() {
            tdutils::log_error!("Receive {guard_bot_user_id:?}");
            guard_bot_user_id = UserId::default();
        }
        if !user_manager.is_user_bot_of(guard_bot_user_id) {
            guard_bot_user_id = UserId::default();
        }
        let Some(f) = self.get_channel_full_mut(channel_id) else {
            return;
        };
        if f.guard_bot_user_id != guard_bot_user_id {
            f.guard_bot_user_id = guard_bot_user_id;
            f.is_changed = true;
        }
    }

    /// `on_update_channel_administrator_count`
    /// (ChatManager.cpp:9091-9110) — keeps the
    /// `participant_count >= administrator_count` invariant outside
    /// on_get_channel_full too.
    pub fn on_update_channel_administrator_count(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        channel_id: ChannelId,
        administrator_count: i32,
    ) {
        if self
            .get_channel_full_force(
                user_manager,
                env,
                channel_id,
                true,
                "on_update_channel_administrator_count",
                false,
            )
            .is_none()
        {
            return;
        }
        if let Some(f) = self.get_channel_full_mut(channel_id)
            && f.administrator_count != administrator_count
        {
            f.administrator_count = administrator_count;
            f.is_changed = true;
            if f.participant_count < f.administrator_count {
                f.participant_count = f.administrator_count;
            }
        }
        let participant_count = self
            .get_channel_full_const(channel_id)
            .map(|f| f.participant_count)
            .unwrap_or_default();
        if let Some(c) = self.get_channel_mut(channel_id)
            && c.participant_count != participant_count
        {
            c.participant_count = participant_count;
            c.is_changed = true;
        }
        self.update_channel_full(
            user_manager,
            env,
            channel_id,
            "on_update_channel_administrator_count",
            false,
        );
    }

    /// `ChatManager::update_channel` (ChatManager.cpp:5346-5547). The
    /// MessagesManager/StoryManager/DialogManager dialog fan-outs and the
    /// SuggestedActionManager/DialogParticipantManager refires are the
    /// documented stubs; `update_created_public_channels`,
    /// `update_dialogs_for_discussion`, `remove_inactive_channel` and the
    /// restricted-id set keep their C++ shapes. The 1.0-second SleepActor
    /// hop around the status-repair reload defers only the net query, so
    /// the interim `reload_channel` call is direct.
    fn update_channel(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        channel_id: ChannelId,
        from_binlog: bool,
        from_database: bool,
    ) {
        if self
            .get_channel(channel_id)
            .is_some_and(|c| c.is_being_updated)
        {
            tdutils::log_error!("Detected recursive update of {channel_id:?}");
        }
        if let Some(c) = self.get_channel_mut(channel_id) {
            c.is_being_updated = true;
        }

        let mut need_update_channel_full = false;
        let photo_changed = self
            .get_channel(channel_id)
            .is_some_and(|c| c.is_photo_changed);
        if photo_changed {
            // td_->messages_manager_->on_dialog_photo_updated (+ the
            // monoforum twin) — the M.10 stub.
            if let Some(c) = self.get_channel_mut(channel_id) {
                c.is_photo_changed = false;
            }
            let full_snapshot = self
                .get_channel_full(env, channel_id, true, "update_channel")
                .map(|f| (f.photo.clone(), f.is_update_channel_full_sent));
            let full_differs = full_snapshot.is_some_and(|(full_photo, _)| {
                !is_same_dialog_photo(
                    &full_photo,
                    &self.get_channel(channel_id).expect("checked above").photo,
                )
            });
            if full_differs {
                self.on_update_channel_full_photo(env, channel_id, Photo::default());
                if self
                    .get_channel_full_const(channel_id)
                    .is_some_and(|f| f.is_update_channel_full_sent)
                {
                    need_update_channel_full = true;
                }
                let small_file_id_valid = self
                    .get_channel(channel_id)
                    .is_some_and(|c| c.photo.small_file_id.is_valid());
                if small_file_id_valid
                    && let Some(f) = self.get_channel_full(env, channel_id, true, "update_channel")
                    && f.expires_at > 0.0
                {
                    f.expires_at = 0.0;
                    f.need_save_to_database = true;
                }
                if small_file_id_valid {
                    self.send_get_channel_full_query(
                        env,
                        channel_id,
                        Promise::default(),
                        "update_channel",
                    );
                }
            }
        }
        let accent_color_changed = self
            .get_channel(channel_id)
            .is_some_and(|c| c.is_accent_color_changed);
        if accent_color_changed {
            // td_->messages_manager_->on_dialog_accent_colors_updated (+ the
            // monoforum twin) — the M.10 stub.
            if let Some(c) = self.get_channel_mut(channel_id) {
                c.is_accent_color_changed = false;
            }
        }
        let title_changed = self
            .get_channel(channel_id)
            .is_some_and(|c| c.is_title_changed);
        if title_changed {
            // td_->messages_manager_->on_dialog_title_updated (+ the
            // monoforum twin) — the M.10 stub.
            if let Some(c) = self.get_channel_mut(channel_id) {
                c.is_title_changed = false;
            }
        }
        // must run before the status branch, like the C++
        let admined_monoforum_changed = self
            .get_channel(channel_id)
            .is_some_and(|c| c.is_admined_monoforum_changed);
        if admined_monoforum_changed {
            let status = self.get_channel(channel_id).map(|c| c.status.clone());
            if let Some(status) = status {
                self.on_update_channel_status(user_manager, env, channel_id, status);
            }
            // send_closure_later(messages_manager on_dialog_deleted) — the
            // M.10 stub.
            if let Some(c) = self.get_channel_mut(channel_id) {
                c.is_admined_monoforum_changed = false;
            }
        }
        // must run after the admined-monoforum branch
        let status_changed = self
            .get_channel(channel_id)
            .is_some_and(|c| c.is_status_changed);
        if status_changed {
            {
                let c = self.get_channel_mut(channel_id).expect("checked above");
                c.status.update_restrictions(env.global);
                let until_date = c.status.get_until_date();
                let mut left_time = 0.0;
                if until_date > 0 {
                    left_time = until_date as f64 - env.global.server_time() + 2.0;
                    if left_time <= 0.0 {
                        c.status.update_restrictions(env.global);
                        debug_assert!(c.status.get_until_date() == 0);
                    }
                }
                if left_time > 0.0 && left_time < 366.0 * 86400.0 {
                    Self::set_timeout(&mut self.channel_unban_timeouts, channel_id, left_time);
                } else {
                    Self::cancel_timeout(&mut self.channel_unban_timeouts, channel_id);
                }
            }
            let suitable = self
                .get_channel(channel_id)
                .is_some_and(Self::is_suitable_discussion_channel);
            self.update_dialogs_for_discussion(DialogId::from_channel_id(channel_id), suitable);
            if !self
                .get_channel(channel_id)
                .is_some_and(|c| c.status.is_member())
            {
                self.remove_inactive_channel(channel_id);
            }
            // td_->messages_manager_->drop_dialog_pending_join_requests —
            // the M.10 stub.
            if !from_database
                && self
                    .get_channel(channel_id)
                    .is_some_and(|c| c.is_update_supergroup_sent)
            {
                // the C++ SleepActor defers only the net query
                self.reload_channel(
                    env,
                    channel_id,
                    Promise::default(),
                    "ReloadChannelSleepActor",
                );
            }
            // td_->messages_manager_->on_dialog_access_updated — the M.10 stub.
            if let Some(c) = self.get_channel_mut(channel_id) {
                c.is_status_changed = false;
            }
        }
        let username_changed = self
            .get_channel(channel_id)
            .is_some_and(|c| c.is_username_changed);
        if username_changed {
            let is_creator = self
                .get_channel(channel_id)
                .is_some_and(|c| c.status.is_creator());
            if is_creator {
                let c = self.get_channel(channel_id).cloned();
                self.update_created_public_channels(env, &c.expect("checked above"), channel_id);
            }
            // td_->messages_manager_->on_dialog_access_updated — the M.10 stub.
            if let Some(c) = self.get_channel_mut(channel_id) {
                c.is_username_changed = false;
            }
        }
        let default_permissions_changed = self
            .get_channel(channel_id)
            .is_some_and(|c| c.is_default_permissions_changed);
        if default_permissions_changed {
            // td_->messages_manager_->on_dialog_default_permissions_updated
            // and the SuggestedActionManager ConvertToGigagroup removal —
            // the M.10/M.13 stubs.
            if let Some(c) = self.get_channel_mut(channel_id) {
                c.is_default_permissions_changed = false;
            }
        }
        let has_location_changed = self
            .get_channel(channel_id)
            .is_some_and(|c| c.is_has_location_changed);
        if has_location_changed {
            let is_creator = self
                .get_channel(channel_id)
                .is_some_and(|c| c.status.is_creator());
            if is_creator {
                let c = self.get_channel(channel_id).cloned();
                self.update_created_public_channels(env, &c.expect("checked above"), channel_id);
            }
            // td_->messages_manager_->on_dialog_access_updated — the M.10 stub.
            if let Some(c) = self.get_channel_mut(channel_id) {
                c.is_has_location_changed = false;
            }
        }
        let creator_changed = self
            .get_channel(channel_id)
            .is_some_and(|c| c.is_creator_changed);
        if creator_changed {
            let c = self.get_channel(channel_id).cloned();
            self.update_created_public_channels(env, &c.expect("checked above"), channel_id);
            if let Some(c) = self.get_channel_mut(channel_id) {
                c.is_creator_changed = false;
            }
        }
        let noforwards_changed = self
            .get_channel(channel_id)
            .is_some_and(|c| c.is_noforwards_changed);
        if noforwards_changed {
            // td_->messages_manager_->on_dialog_has_protected_content_updated
            // — the M.10 stub.
            if let Some(c) = self.get_channel_mut(channel_id) {
                c.is_noforwards_changed = false;
            }
        }
        let is_forum_changed = self
            .get_channel(channel_id)
            .is_some_and(|c| c.is_is_forum_changed);
        if is_forum_changed {
            let suitable = self
                .get_channel(channel_id)
                .is_some_and(Self::is_suitable_discussion_channel);
            self.update_dialogs_for_discussion(DialogId::from_channel_id(channel_id), suitable);
            // td_->messages_manager_->on_update_dialog_is_forum — the M.10 stub.
            if let Some(c) = self.get_channel_mut(channel_id) {
                c.is_is_forum_changed = false;
            }
        }
        let stories_hidden_changed = self
            .get_channel(channel_id)
            .is_some_and(|c| c.is_stories_hidden_changed);
        if stories_hidden_changed {
            // send_closure_later(story_manager on_dialog_active_stories_order_updated)
            // — the StoryManager stub.
            if let Some(c) = self.get_channel_mut(channel_id) {
                c.is_stories_hidden_changed = false;
            }
        }
        let unix_time = env.global.unix_time();
        let effective_emoji_status = EmojiStatus::get_effective_emoji_status(
            &self
                .get_channel(channel_id)
                .and_then(|c| c.emoji_status.clone()),
            true,
            unix_time,
        );
        let emoji_status_changed = self
            .get_channel(channel_id)
            .is_some_and(|c| effective_emoji_status != c.last_sent_emoji_status);
        if emoji_status_changed {
            let had_last_sent = self
                .get_channel(channel_id)
                .is_some_and(|c| c.last_sent_emoji_status.is_some());
            if had_last_sent {
                Self::cancel_timeout(&mut self.channel_emoji_status_timeouts, channel_id);
            }
            let armed_timeout = {
                let c = self.get_channel_mut(channel_id).expect("checked above");
                c.last_sent_emoji_status = effective_emoji_status;
                c.last_sent_emoji_status.as_ref().and_then(|last_sent| {
                    let left_time = i64::from(last_sent.get_until_date()) - i64::from(unix_time);
                    (0..30 * 86400)
                        .contains(&left_time)
                        .then_some(left_time as f64)
                })
            };
            if let Some(timeout_in) = armed_timeout {
                Self::set_timeout(
                    &mut self.channel_emoji_status_timeouts,
                    channel_id,
                    timeout_in,
                );
            }
            // td_->messages_manager_->on_dialog_emoji_status_updated (+ the
            // monoforum twin) — the M.10 stub.
        }
        if let Some(c) = self.get_channel_mut(channel_id) {
            c.is_emoji_status_changed = false;
        }

        if !env.auth_is_bot {
            let restrictions_empty = self
                .get_channel(channel_id)
                .is_some_and(|c| c.restriction_reasons.is_empty());
            if restrictions_empty {
                self.restricted_channel_ids.remove(&channel_id.get());
            } else {
                self.restricted_channel_ids.insert(channel_id.get());
            }
        }

        // from_binlog || from_database → dialog_manager
        // on_dialog_usernames_received — the M.6 stub.
        // !is_channel_public(c) && !c->has_linked_channel → messages_manager
        // on_update_dialog_default_send_message_as_dialog_id — the M.10 stub.

        if need_update_channel_full {
            debug_assert!(self.get_channel_full_const(channel_id).is_some());
            self.update_channel_full(user_manager, env, channel_id, "update_channel", false);
        }

        let changed = {
            let Some(c) = self.get_channel_mut(channel_id) else {
                return;
            };
            tdutils::log_debug!(
                "Update {channel_id:?}: need_save_to_database = {}, is_changed = {}",
                c.need_save_to_database,
                c.is_changed
            );
            c.need_save_to_database |= c.is_changed;
            if c.need_save_to_database {
                if !from_database {
                    c.is_saved = false;
                }
                c.need_save_to_database = false;
            }
            let changed = c.is_changed;
            if changed {
                c.is_changed = false;
                c.is_update_supergroup_sent = true;
            }
            changed
        };
        if changed {
            let c_snapshot = self.get_channel(channel_id).cloned();
            let update = self.get_update_supergroup_object(channel_id, c_snapshot.as_ref());
            self.pending_updates.push(update);
        }
        if !from_database {
            self.save_channel(env, channel_id, from_binlog);
        }
        if let Some(c) = self.get_channel_mut(channel_id) {
            c.is_being_updated = false;
        }
        let have_read_access = self.have_input_peer_channel_id(env, channel_id, AccessRights::Read);
        if let Some(c) = self.get_channel_mut(channel_id) {
            if c.had_read_access && !have_read_access {
                // send_closure_later(messages_manager on_dialog_deleted) —
                // the M.10 stub.
            }
            c.had_read_access = have_read_access;
        }
        let need_repair = self
            .get_channel(channel_id)
            .is_some_and(|c| c.cache_version != Channel::CACHE_VERSION && !c.is_repaired)
            && self.have_input_peer_channel_id(env, channel_id, AccessRights::Read)
            && !env.global.close_flag();
        if need_repair {
            if let Some(c) = self.get_channel_mut(channel_id) {
                c.is_repaired = true;
            }
            tdutils::log_info!("Repairing cache of {channel_id:?}");
            self.reload_channel(env, channel_id, Promise::default(), "update_channel");
        }
    }

    /// `is_suitable_discussion_channel` (ChatManager.cpp:4092-4096).
    fn is_suitable_discussion_channel(c: &Channel) -> bool {
        c.is_megagroup
            && c.status.is_administrator()
            && c.status.can_pin_messages()
            && !c.is_monoforum
            && !c.is_forum
    }

    /// `is_suitable_created_public_channel` (ChatManager.cpp:4101-4115).
    fn is_suitable_created_public_channel(
        public_dialog_type: PublicDialogType,
        c: Option<&Channel>,
    ) -> bool {
        let Some(c) = c else {
            return false;
        };
        if !c.status.is_creator() {
            return false;
        }
        match public_dialog_type {
            PublicDialogType::HasUsername => c.usernames.has_editable_username(),
            PublicDialogType::IsLocationBased => c.has_location,
            PublicDialogType::ForPersonalDialog => {
                !c.is_megagroup && c.usernames.has_first_username()
            }
        }
    }

    /// `update_created_public_channels` (ChatManager.cpp:4195-4216). The
    /// `reload_created_public_dialogs` refire is the net query — the
    /// interim logs and skips it.
    fn update_created_public_channels(
        &mut self,
        env: &mut UserEnv,
        c: &Channel,
        channel_id: ChannelId,
    ) {
        for public_dialog_type in [
            PublicDialogType::HasUsername,
            PublicDialogType::IsLocationBased,
            PublicDialogType::ForPersonalDialog,
        ] {
            let index = public_dialog_type as usize;
            if !self.created_public_channels_inited[index] {
                continue;
            }
            let was_changed;
            if !Self::is_suitable_created_public_channel(public_dialog_type, Some(c)) {
                let old_len = self.created_public_channels[index].len();
                self.created_public_channels[index].retain(|id| *id != channel_id.get());
                was_changed = self.created_public_channels[index].len() != old_len;
            } else if !self.created_public_channels[index].contains(&channel_id.get()) {
                self.created_public_channels[index].push(channel_id.get());
                was_changed = true;
            } else {
                was_changed = false;
            }
            if was_changed {
                self.save_created_public_channels(public_dialog_type, env);
                // reload_created_public_dialogs — the GetCreatedPublicChannels
                // query is [blocked: net] (M.5.46).
                tdutils::log_info!(
                    "GetCreatedPublicChannelsQuery for {public_dialog_type:?} — [blocked: net]"
                );
            }
        }
    }

    /// `save_created_public_channels` (ChatManager.cpp:4232-4245).
    fn save_created_public_channels(
        &mut self,
        public_dialog_type: PublicDialogType,
        env: &mut UserEnv,
    ) {
        let index = public_dialog_type as usize;
        debug_assert!(self.created_public_channels_inited[index]);
        if !env.global.use_message_database() {
            return;
        }
        let joined = self.created_public_channels[index]
            .iter()
            .map(|id| id.to_string())
            .collect::<Vec<_>>()
            .join(",");
        env.global
            .td_db()
            .binlog_pmc()
            .set(&format!("public_channels{index}"), &joined);
    }

    /// `remove_inactive_channel` (ChatManager.cpp:4370-4375).
    fn remove_inactive_channel(&mut self, channel_id: ChannelId) {
        if self.inactive_channel_ids_inited
            && let Some(position) = self
                .inactive_channel_ids
                .iter()
                .position(|id| *id == channel_id.get())
        {
            self.inactive_channel_ids.remove(position);
            tdutils::log_debug!("Remove {channel_id:?} from list of inactive channels");
        }
    }

    /// `on_update_channel_status` (ChatManager.cpp:7710-7736).
    fn on_update_channel_status(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        channel_id: ChannelId,
        mut status: DialogParticipantStatus,
    ) {
        let Some(c) = self.get_channel(channel_id) else {
            return;
        };
        if c.is_monoforum {
            if status.is_member() {
                // monoforums have no member tags
                status = if c.is_admined_monoforum && !env.auth_is_bot {
                    DialogParticipantStatus::administrator(
                        AdministratorRights::from_bools(
                            true,
                            true,
                            false,
                            false,
                            false,
                            false,
                            false,
                            false,
                            false,
                            false,
                            false,
                            false,
                            false,
                            false,
                            false,
                            false,
                            false,
                            false,
                            false,
                            ChannelType::Megagroup,
                        ),
                        String::new(),
                        false,
                    )
                } else {
                    DialogParticipantStatus::member(0, String::new())
                };
            } else {
                status = DialogParticipantStatus::left();
            }
        }
        if c.status != status {
            tdutils::log_info!(
                "Update {channel_id:?} status from {:?} to {:?}",
                c.status,
                status
            );
            if c.is_update_supergroup_sent {
                let old_status = c.status.clone();
                self.on_channel_status_changed(user_manager, env, channel_id, &old_status, &status);
            }
            let c = self.get_channel_mut(channel_id).expect("checked above");
            c.status = status;
            c.is_status_changed = true;
            c.is_changed = true;
        }
    }

    /// `on_channel_status_changed` (ChatManager.cpp:7738-7804). The
    /// StoryManager/WelcomeMessageManager/DialogParticipantManager/
    /// SuggestedActionManager/DialogInviteLinkManager fan-outs are the
    /// documented stubs.
    fn on_channel_status_changed(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        channel_id: ChannelId,
        old_status: &DialogParticipantStatus,
        new_status: &DialogParticipantStatus,
    ) {
        debug_assert!(
            self.get_channel(channel_id)
                .is_some_and(|c| c.is_update_supergroup_sent)
        );
        let have_channel_full = self.get_channel_full_const(channel_id).is_some();

        let need_reload_group_call = old_status.can_manage_calls() != new_status.can_manage_calls();
        if old_status.can_manage_invite_links() && !new_status.can_manage_invite_links() {
            if self.get_channel_full_const(channel_id).is_some() {
                // otherwise the invite link drops when the channel loads
                self.on_update_channel_full_invite_link(env, channel_id, None);
                let need_drop_slow_mode_delay = !self
                    .get_channel(channel_id)
                    .is_some_and(|c| c.is_slow_mode_enabled);
                self.do_invalidate_channel_full(channel_id, need_drop_slow_mode_delay);
                self.update_channel_full(
                    user_manager,
                    env,
                    channel_id,
                    "on_channel_status_changed",
                    false,
                );
            }
        } else {
            let need_drop_slow_mode_delay = !self
                .get_channel(channel_id)
                .is_some_and(|c| c.is_slow_mode_enabled);
            self.invalidate_channel_full(
                user_manager,
                env,
                channel_id,
                need_drop_slow_mode_delay,
                "on_channel_status_changed",
            );
        }
        // the WelcomeMessageManager drop — the wave-3 stub.
        let _ = old_status.can_change_info_and_settings_as_administrator()
            && !new_status.can_change_info_and_settings_as_administrator();

        if old_status.is_creator() != new_status.is_creator() {
            if let Some(c) = self.get_channel_mut(channel_id) {
                c.is_creator_changed = true;
            }
            // send_get_channel_full_query(nullptr, ...) — the query the
            // owner change refires is [blocked: net] (M.5.45).
            tdutils::log_info!("GetFullChannelQuery after the owner change — [blocked: net]");
            // the DialogParticipantManager/SuggestedActionManager refires —
            // the wave-2 stubs.
        }

        if old_status.is_member() != new_status.is_member() || new_status.is_banned() {
            // td_->dialog_invite_link_manager_->remove_dialog_access_by_invite_link
            // — the wave-2 stub.
            if new_status.is_member() || new_status.is_creator() {
                self.reload_channel_full(
                    env,
                    channel_id,
                    Promise::default(),
                    "on_channel_status_changed",
                );
            }
        }
        // the group-call rights, participant-cache, dialog-deleted and
        // expiring-stories refires — the M.10/M.12 stubs.
        let _ = need_reload_group_call;

        // must not load ChannelFull, because must not change the Channel
        debug_assert!(have_channel_full == self.get_channel_full_const(channel_id).is_some());
    }

    // ---- M.5.20: the telegram_api::Chat application matrix ----

    /// `need_update_dialog_photo` (DialogPhoto.cpp:212-215) — the small
    /// and big file ids plus the animation and personal flags; the
    /// minithumbnail is compared separately by [`Self::on_update_chat_photo`].
    fn need_update_dialog_photo(from: &DialogPhoto, to: &DialogPhoto) -> bool {
        from.small_file_id != to.small_file_id
            || from.big_file_id != to.big_file_id
            || from.has_animation != to.has_animation
            || from.is_personal != to.is_personal
    }

    /// `on_update_chat_title` (ChatManager.cpp:7483-7487).
    fn on_update_chat_title(c: &mut Chat, chat_id: ChatId, title: String) {
        let _ = chat_id;
        if c.title != title {
            c.title = title;
            c.is_title_changed = true;
            c.need_save_to_database = true;
        }
    }

    /// `on_update_chat_active` (ChatManager.cpp:7491-7495).
    fn on_update_chat_active(c: &mut Chat, chat_id: ChatId, is_active: bool) {
        if c.is_active != is_active {
            c.is_active = is_active;
            c.is_is_active_changed = true;
            c.is_changed = true;
        }
        let _ = chat_id;
    }

    /// `on_update_chat_migrated_to_channel_id` (ChatManager.cpp:7499-7505)
    /// — an invalid new id never overwrites the stored one.
    fn on_update_chat_migrated_to_channel_id(
        c: &mut Chat,
        chat_id: ChatId,
        migrated_to_channel_id: ChannelId,
    ) {
        if c.migrated_to_channel_id != migrated_to_channel_id && migrated_to_channel_id.is_valid() {
            if c.migrated_to_channel_id.is_valid() {
                tdutils::log_error!(
                    "Upgraded supergroup ID for {chat_id:?} has changed from {:?} to {migrated_to_channel_id:?}",
                    c.migrated_to_channel_id
                );
            }
            c.migrated_to_channel_id = migrated_to_channel_id;
            c.is_changed = true;
        }
    }

    /// `on_update_chat_noforwards` (ChatManager.cpp:7357-7363).
    fn on_update_chat_noforwards(c: &mut Chat, chat_id: ChatId, noforwards: bool) {
        if c.noforwards != noforwards {
            tdutils::log_info!(
                "{chat_id:?} has_protected_content changed from {} to {noforwards}",
                c.noforwards
            );
            c.noforwards = noforwards;
            c.is_noforwards_changed = true;
            c.need_save_to_database = true;
        }
    }

    /// `on_update_chat_default_permissions` (ChatManager.cpp:7345-7354),
    /// the `Chat *` form. The `ChatId` overload belongs to the
    /// updateChatDefaultBannedRights intake — a later slice.
    fn on_update_chat_default_permissions(
        c: &mut Chat,
        chat_id: ChatId,
        default_permissions: RestrictedRights,
        version: i32,
    ) {
        if c.default_permissions != default_permissions && version >= c.default_permissions_version
        {
            tdutils::log_info!("{chat_id:?} default permissions changed");
            c.default_permissions = default_permissions;
            c.default_permissions_version = version;
            c.is_default_permissions_changed = true;
            c.need_save_to_database = true;
        }
    }

    /// `on_update_chat_status` (ChatManager.cpp:7258-7292). The left
    /// branch zeroes the participant count and invalidates all three
    /// independent version counters at once.
    fn on_update_chat_status(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        chat_id: ChatId,
        status: DialogParticipantStatus,
    ) {
        let Some(c) = self.get_chat_mut(chat_id) else {
            return;
        };
        if c.status == status {
            return;
        }
        tdutils::log_info!(
            "Update {chat_id:?} status from {:?} to {status:?}",
            c.status
        );
        let old_status = c.status.clone();
        let need_reload_group_call = old_status.can_manage_calls() != status.can_manage_calls();
        let need_drop_invite_link =
            old_status.can_manage_invite_links() && !status.can_manage_invite_links();
        let need_drop_welcome_messages = old_status.can_change_info_and_settings_as_administrator()
            && !status.can_change_info_and_settings_as_administrator();
        let is_left = status.is_left();
        c.status = status;
        c.is_status_changed = true;
        if is_left {
            c.participant_count = 0;
            c.version = -1;
            c.default_permissions_version = -1;
            c.pinned_message_version = -1;
        }
        if is_left {
            self.drop_chat_full(user_manager, env, chat_id);
        } else if need_drop_invite_link
            && self
                .get_chat_full_force(user_manager, env, chat_id, "on_update_chat_status")
                .is_some()
        {
            self.on_update_chat_full_invite_link(env, chat_id, None);
            self.update_chat_full(user_manager, env, chat_id, "on_update_chat_status", false);
        }
        if need_reload_group_call {
            // td_->messages_manager_->on_update_dialog_group_call_rights —
            // the M.10 stub.
        }
        if need_drop_welcome_messages {
            // td_->welcome_message_manager_->drop_welcome_messages — the
            // wave-3 stub.
        }
        let c = self.get_chat_mut(chat_id).expect("checked above");
        c.is_changed = true;
    }

    /// `on_update_chat_participant_count` (ChatManager.cpp:7411-7443) —
    /// the version machine: stale versions answer silently, the
    /// deleted-user leave keeps the version and repairs instead.
    fn on_update_chat_participant_count(
        &mut self,
        env: &mut UserEnv,
        chat_id: ChatId,
        participant_count: i32,
        version: i32,
        debug_str: &str,
    ) {
        let Some(c) = self.get_chat_mut(chat_id) else {
            return;
        };
        let mut needs_repair = false;
        if version <= -1 {
            tdutils::log_error!("Receive wrong version {version} in {chat_id:?}{debug_str}");
            return;
        }
        if version < c.version {
            tdutils::log_info!(
                "Receive number of members in {chat_id:?} with version {version}{debug_str}, but current version is {}",
                c.version
            );
            return;
        }
        if c.participant_count != participant_count {
            if version == c.version && participant_count != 0 {
                // version is not changed when a deleted user is removed
                if c.participant_count != participant_count + 1 {
                    tdutils::log_error!(
                        "Number of members in {chat_id:?} has changed from {} to {participant_count}, but version {version} remains unchanged{debug_str}",
                        c.participant_count
                    );
                }
                needs_repair = true;
            }
            c.participant_count = participant_count;
            c.version = version;
            c.is_changed = true;
            if needs_repair {
                self.repair_chat_participants(env, chat_id);
            }
            return;
        }
        if version > c.version {
            c.version = version;
            c.need_save_to_database = true;
        }
    }

    /// `repair_chat_participants` (ChatManager.cpp:6995-6997) — re-requests
    /// the full through GetFullChat; [blocked: net] (M.5.44).
    fn repair_chat_participants(&mut self, env: &mut UserEnv, chat_id: ChatId) {
        self.send_get_chat_full_query(env, chat_id, Promise::default(), "repair_chat_participants");
    }

    /// `on_get_chat_participants` (ChatManager.cpp:6273-6344).
    pub fn on_get_chat_participants(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        participants: telegram_api::ChatParticipants,
        from_update: bool,
    ) {
        match participants {
            telegram_api::ChatParticipants::chatParticipantsForbidden(p) => {
                let chat_id = ChatId::new(p.chat_id);
                if !chat_id.is_valid() {
                    tdutils::log_error!("Receive invalid {chat_id:?}");
                    return;
                }
                if !self.have_chat_force(user_manager, env, chat_id, "on_get_chat_participants") {
                    tdutils::log_error!("{chat_id:?} not found");
                    return;
                }
                if from_update {
                    self.drop_chat_full(user_manager, env, chat_id);
                }
            }
            telegram_api::ChatParticipants::chatParticipants(p) => {
                let chat_id = ChatId::new(p.chat_id);
                if !chat_id.is_valid() {
                    tdutils::log_error!("Receive invalid {chat_id:?}");
                    return;
                }
                if self
                    .get_chat_force(user_manager, env, chat_id, "chatParticipants")
                    .is_none()
                {
                    tdutils::log_error!("{chat_id:?} not found");
                    return;
                }
                // snapshot the record fields the participant conversion needs
                let (chat_date, chat_is_creator) = {
                    let c = self.get_chat(chat_id).expect("force-loaded above");
                    (c.date, c.status.is_creator())
                };
                if self
                    .get_chat_full_force(
                        user_manager,
                        env,
                        chat_id,
                        "telegram_api::chatParticipants",
                    )
                    .is_none()
                {
                    tdutils::log_info!("Ignore update of members for unknown full {chat_id:?}");
                    return;
                }

                let mut new_creator_user_id = UserId::default();
                let mut new_participants = Vec::with_capacity(p.participants.len());
                for participant_ptr in p.participants {
                    let mut dialog_participant = DialogParticipant::from_chat_participant(
                        participant_ptr,
                        chat_date,
                        chat_is_creator,
                    );
                    if !dialog_participant.is_valid() {
                        tdutils::log_error!("Receive invalid {dialog_participant:?}");
                        continue;
                    }
                    // LOG_IF(ERROR, !have_dialog_info) — the DialogManager
                    // stub (M.6) answers from local caches; emit nothing.
                    let inviter = dialog_participant.inviter_user_id_;
                    if !user_manager.have_min_user(inviter) {
                        tdutils::log_error!(
                            "Have no information about {inviter:?} as a member of {chat_id:?}"
                        );
                    }
                    if dialog_participant.joined_date_ < chat_date {
                        if dialog_participant.joined_date_ < chat_date - 30
                            && chat_date >= 1_486_000_000
                        {
                            tdutils::log_error!(
                                "Wrong join date = {} for {:?}, {chat_id:?} was created at {chat_date}",
                                dialog_participant.joined_date_,
                                dialog_participant.dialog_id_
                            );
                        }
                        dialog_participant.joined_date_ = chat_date;
                    }
                    if dialog_participant.status_.is_creator()
                        && dialog_participant.dialog_id_.get_type() == DialogType::User
                    {
                        new_creator_user_id = dialog_participant.dialog_id_.get_user_id();
                    }
                    new_participants.push(dialog_participant);
                }

                if let Some(f) = self.chats_full.get_mut(&chat_id.get())
                    && f.creator_user_id != new_creator_user_id
                {
                    f.creator_user_id = new_creator_user_id;
                    f.is_changed = true;
                }
                self.on_update_chat_full_participants(
                    user_manager,
                    env,
                    chat_id,
                    new_participants,
                    p.version,
                    from_update,
                );
                if from_update {
                    self.update_chat_full(
                        user_manager,
                        env,
                        chat_id,
                        "on_get_chat_participants",
                        false,
                    );
                }
            }
        }
    }

    /// `on_update_chat_full_participants` (ChatManager.cpp:7548-7574).
    fn on_update_chat_full_participants(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        chat_id: ChatId,
        participants: Vec<DialogParticipant>,
        version: i32,
        from_update: bool,
    ) {
        if version <= -1 {
            tdutils::log_error!("Receive members with wrong version {version} in {chat_id:?}");
            return;
        }
        let Some(f) = self.chats_full.get_mut(&chat_id.get()) else {
            debug_assert!(
                false,
                "Missing full {chat_id:?} in on_update_chat_full_participants"
            );
            return;
        };
        if version < f.version {
            tdutils::log_warning!(
                "Receive members of {chat_id:?} with version {version} but current version is {}",
                f.version
            );
            return;
        }
        if (f.participants.len() != participants.len() && version == f.version)
            || (from_update && version != f.version + 1)
        {
            tdutils::log_info!("Members of {chat_id:?} has changed");
            self.repair_chat_participants(env, chat_id);
            return;
        }
        if let Some(f) = self.chats_full.get_mut(&chat_id.get()) {
            f.participants = participants;
            f.version = version;
            f.is_changed = true;
        }
        self.update_chat_online_member_count_full(user_manager, env, chat_id, true);
    }

    /// `update_chat_online_member_count(ChatFull *, ...)` — the
    /// DialogParticipantManager counter is the wave-2 stub.
    fn update_chat_online_member_count_full(
        &mut self,
        _user_manager: &mut UserManager,
        _env: &mut UserEnv,
        _chat_id: ChatId,
        _is_from_server: bool,
    ) {
    }

    /// `get_chat_participant` (ChatManager.cpp:6358-6363) — the local
    /// accessor.
    #[allow(dead_code)] // promise form lands with the net step (M.5.44)
    fn get_chat_participant(&self, chat_id: ChatId, user_id: UserId) -> Option<&DialogParticipant> {
        let chat_full = self.get_chat_full(chat_id)?;
        let dialog_id = DialogId::from_user_id(user_id);
        self.get_chat_full_participant(chat_full, dialog_id)
    }

    /// `get_chat_full_participant` (ChatManager.cpp:6366-6373).
    #[allow(dead_code)] // consumed through get_chat_participant (M.5.44)
    fn get_chat_full_participant<'a>(
        &self,
        chat_full: &'a ChatFull,
        dialog_id: DialogId,
    ) -> Option<&'a DialogParticipant> {
        chat_full
            .participants
            .iter()
            .find(|p| p.dialog_id_ == dialog_id)
    }

    /// `get_chat_participants` (ChatManager.cpp:6375-6381).
    #[allow(dead_code)] // read-surface callers land with M.5.36/M.5.47
    pub(crate) fn get_chat_participants(&self, chat_id: ChatId) -> Option<&Vec<DialogParticipant>> {
        self.get_chat_full(chat_id).map(|f| &f.participants)
    }

    /// `CHANNEL_FULL_EXPIRE_TIME` (ChatManager.h:683).
    const CHANNEL_FULL_EXPIRE_TIME: i32 = 60;

    /// `on_get_chat_full` (ChatManager.cpp:5762-6234) — both halves in one
    /// dispatch. MessagesManager/DialogManager/StoryManager/GroupCallManager/
    /// StickersManager/SuggestedActionManager fan-outs are the documented
    /// stubs.
    pub fn on_get_chat_full(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        chat_full_ptr: telegram_api::ChatFull,
        mut promise: Promise<()>,
    ) {
        match chat_full_ptr {
            telegram_api::ChatFull::chatFull(mut chat) => {
                // ---- the basic-group half (cpp:5762-6049) ----
                let chat_id = ChatId::new(chat.id);
                if self.get_chat(chat_id).is_none() {
                    tdutils::log_error!("Can't find {chat_id:?}");
                    return promise.set_value(());
                }
                // the pinned-message machine, forward only
                if self
                    .get_chat(chat_id)
                    .is_some_and(|c| c.version >= c.pinned_message_version)
                {
                    let pinned_message_id = MessageId::new(i64::from(chat.pinned_msg_id));
                    let (version, pinned_version) = {
                        let c = self.get_chat(chat_id).unwrap();
                        (c.version, c.pinned_message_version)
                    };
                    tdutils::log_info!(
                        "Receive pinned {pinned_message_id:?} in {chat_id:?} with version {version}. Current version is {pinned_version}"
                    );
                    // td_->messages_manager_->on_update_dialog_last_pinned_message_id
                    // — the M.6 stub.
                    if version > pinned_version {
                        if let Some(c) = self.get_chat_mut(chat_id) {
                            c.pinned_message_version = c.version;
                            c.need_save_to_database = true;
                        }
                        self.update_chat(user_manager, env, chat_id, false, false);
                    }
                }
                // folder id (flags.11), scheduled messages, welcome messages,
                // group call (flags.12), ttl (flags.14), translatable, the
                // default join-as (flags.15) — the MessagesManager fan-outs;
                // all M.6 stubs. welcome drop — the wave-2 stub.
                self.add_chat_full(chat_id);
                self.on_update_chat_full_invite_link(
                    env,
                    chat_id,
                    // the C++ `std::move(chat->exported_invite_)` — the
                    // nullable pointer (None when the flag is absent)
                    std::mem::take(&mut chat.exported_invite),
                );
                let photo = crate::photo::get_photo(
                    std::mem::take(&mut chat.chat_photo).unwrap_or_default(),
                );
                // on_update_chat_photo(c, chat_id, as_dialog_photo(..., 0,
                // photo, false), false) — the chat-photo diff machine
                // (M.5.29); a no-op on consistent server data.
                self.on_update_chat_full_photo(env, chat_id, photo);
                {
                    let Some(f) = self.get_chat_full_mut(chat_id) else {
                        return promise.set_value(());
                    };
                    if f.description != chat.about {
                        f.description = chat.about;
                        f.is_changed = true;
                    }
                    // td_->group_call_manager_->on_update_dialog_about — the
                    // wave-2 stub.
                    if f.can_set_username != chat.can_set_username {
                        // need_save_to_database, NOT is_changed
                        f.can_set_username = chat.can_set_username;
                        f.need_save_to_database = true;
                    }
                }
                let participants = std::mem::take(&mut chat.participants).unwrap_or_default();
                self.on_get_chat_participants(user_manager, env, participants, false);
                // notify settings, available reactions (flags.18/20), chat
                // theme (flags.16), pending join requests (flags.17) — the
                // MessagesManager fan-outs; all M.6 stubs.
                let bot_commands = user_manager.get_bot_commands(env, chat.bot_info, None);
                if let Some(f) = self.get_chat_full_mut(chat_id)
                    && f.bot_commands != bot_commands
                {
                    f.bot_commands = bot_commands;
                    f.is_changed = true;
                }
                if self.get_chat(chat_id).is_some_and(|c| c.is_changed) {
                    // with the chat-photo diff machine stubbed, is_changed on
                    // the chat can only be pre-existing state
                    tdutils::log_error!(
                        "Receive inconsistent chatPhoto and chatPhotoInfo for {chat_id:?}"
                    );
                    self.update_chat(user_manager, env, chat_id, false, false);
                }
                if let Some(f) = self.get_chat_full_mut(chat_id) {
                    f.is_update_chat_full_sent = true;
                }
                self.update_chat_full(user_manager, env, chat_id, "on_get_chat_full", false);
            }
            telegram_api::ChatFull::channelFull(mut channel) => {
                // ---- the channel half (cpp:6050-6234) ----
                let channel_id = ChannelId::new(channel.id);
                if self.get_channel(channel_id).is_none() {
                    tdutils::log_error!("Can't find {channel_id:?}");
                    return promise.set_value(());
                }
                self.invalidated_channels_full.remove(&channel_id.get());
                if !env.global.close_flag() {
                    let resend = {
                        let Some(f) = self.get_channel_full_mut(channel_id) else {
                            return promise.set_value(());
                        };
                        if f.repair_request_version != 0
                            && f.repair_request_version < f.speculative_version
                        {
                            tdutils::log_info!(
                                "Receive ChannelFull with request version {}, but current speculative version is {}",
                                f.repair_request_version,
                                f.speculative_version
                            );
                            f.repair_request_version = f.speculative_version;
                            true
                        } else {
                            f.repair_request_version = 0;
                            false
                        }
                    };
                    if resend {
                        debug_assert!(self.get_input_channel(env, channel_id).is_some());
                        return self.send_get_channel_full_query(
                            env,
                            channel_id,
                            promise,
                            "on_get_channel_full",
                        );
                    }
                }
                // notify settings, background (flags2.7), available reactions
                // (flags.30/flags2.13/16), chat theme (flags.27), pending join
                // requests (flags.28), message ttl (flags.24), view-as-messages
                // (flags2.6), translatable (flags2.3), stories (flags2.4) —
                // the MessagesManager/StoryManager fan-outs; all M.6/M.12 stubs.
                self.add_channel_full(channel_id);
                let is_megagroup;
                let is_administrator;
                {
                    let Some(c) = self.get_channel(channel_id) else {
                        return promise.set_value(());
                    };
                    is_megagroup = c.is_megagroup;
                    is_administrator = c.status.is_administrator();
                }
                let have_participant_count = channel.flags & 1 != 0;
                let mut participant_count = if have_participant_count {
                    channel.participants_count
                } else {
                    self.get_channel_full_const(channel_id)
                        .map(|f| f.participant_count)
                        .unwrap_or_default()
                };
                let administrator_count = if channel.flags & (1 << 1) != 0 {
                    channel.admins_count
                } else if is_megagroup || is_administrator {
                    self.get_channel_full_const(channel_id)
                        .map(|f| f.administrator_count)
                        .unwrap_or_default()
                } else {
                    0
                };
                if participant_count < administrator_count {
                    participant_count = administrator_count;
                }
                // the C++ name swap: restricted_count = banned_count_,
                // banned_count = kicked_count_
                let restricted_count = channel.banned_count;
                let banned_count = channel.kicked_count;
                let can_get_participants = channel.can_view_participants;
                let has_hidden_participants = channel.participants_hidden;
                let can_set_sticker_set = channel.can_set_stickers;
                let can_set_location = channel.can_set_location;
                let is_all_history_available = !channel.hidden_prehistory;
                let can_have_sponsored_messages = !channel.restricted_sponsored;
                let has_aggressive_anti_spam_enabled = channel.antispam;
                let mut can_view_statistics = channel.can_view_stats;
                let can_view_revenue = channel.can_view_revenue;
                let has_pinned_stories = channel.stories_pinned_available;
                let mut stats_dc_id = DcId::default();
                if channel.flags & (1 << 12) != 0 {
                    stats_dc_id = DcId::create(channel.stats_dc);
                }
                if !stats_dc_id.is_exact() && can_view_statistics {
                    tdutils::log_error!(
                        "Receive can_view_statistics == true, but invalid statistics DC ID in {channel_id:?}"
                    );
                    can_view_statistics = false;
                }
                let channel_type =
                    Self::get_channel_type(self.get_channel(channel_id).expect("checked above"));
                let main_profile_tab =
                    get_profile_tab(std::mem::take(&mut channel.main_tab), channel_type);
                {
                    let Some(f) = self.get_channel_full_mut(channel_id) else {
                        return promise.set_value(());
                    };
                    f.repair_request_version = 0;
                    f.expires_at = tdutils::time::now() + f64::from(Self::CHANNEL_FULL_EXPIRE_TIME);
                    let changed = f.participant_count != participant_count
                        || f.administrator_count != administrator_count
                        || f.restricted_count != restricted_count
                        || f.banned_count != banned_count
                        || f.can_get_participants != can_get_participants
                        || f.can_set_sticker_set != can_set_sticker_set
                        || f.can_set_location != can_set_location
                        || f.can_view_statistics != can_view_statistics
                        || f.stats_dc_id != stats_dc_id
                        || f.is_all_history_available != is_all_history_available
                        || f.can_have_sponsored_messages != can_have_sponsored_messages
                        || f.has_aggressive_anti_spam_enabled != has_aggressive_anti_spam_enabled
                        || f.has_hidden_participants != has_hidden_participants
                        || f.has_pinned_stories != has_pinned_stories
                        || f.can_view_revenue != can_view_revenue
                        || f.has_paid_media_allowed != channel.paid_media_allowed
                        || f.can_view_star_revenue != channel.can_view_stars_revenue
                        || f.has_stargifts_available != channel.stargifts_available
                        || f.has_paid_messages_available != channel.paid_messages_available
                        || f.send_paid_message_stars != channel.send_paid_messages_stars
                        || f.main_profile_tab != main_profile_tab;
                    if changed {
                        f.participant_count = participant_count;
                        f.administrator_count = administrator_count;
                        f.restricted_count = restricted_count;
                        f.banned_count = banned_count;
                        f.can_get_participants = can_get_participants;
                        f.can_set_sticker_set = can_set_sticker_set;
                        f.can_set_location = can_set_location;
                        f.can_view_statistics = can_view_statistics;
                        f.stats_dc_id = stats_dc_id;
                        f.is_all_history_available = is_all_history_available;
                        f.can_have_sponsored_messages = can_have_sponsored_messages;
                        f.has_aggressive_anti_spam_enabled = has_aggressive_anti_spam_enabled;
                        f.has_hidden_participants = has_hidden_participants;
                        f.has_pinned_stories = has_pinned_stories;
                        f.can_view_revenue = can_view_revenue;
                        f.has_paid_media_allowed = channel.paid_media_allowed;
                        f.can_view_star_revenue = channel.can_view_stars_revenue;
                        f.has_stargifts_available = channel.stargifts_available;
                        f.has_paid_messages_available = channel.paid_messages_available;
                        f.send_paid_message_stars = channel.send_paid_messages_stars;
                        f.main_profile_tab = main_profile_tab;
                        f.is_changed = true;
                    }
                }
                // td_->group_call_manager_->on_update_dialog_about — the
                // wave-2 stub.
                if let Some(f) = self.get_channel_full_mut(channel_id)
                    && f.description != channel.about
                {
                    f.description = channel.about;
                    f.is_changed = true;
                }
                if have_participant_count {
                    if let Some(c) = self.get_channel_mut(channel_id)
                        && c.participant_count != participant_count
                    {
                        c.participant_count = participant_count;
                        c.is_changed = true;
                    }
                    self.update_channel(user_manager, env, channel_id, false, false);
                }
                if let Some(f) = self.get_channel_full_mut(channel_id)
                    && !f.is_can_view_statistics_inited
                {
                    f.is_can_view_statistics_inited = true;
                    f.need_save_to_database = true;
                }
                if let Some(f) = self.get_channel_full_mut(channel_id)
                    && f.can_set_username != channel.can_set_username
                {
                    // save-only
                    f.can_set_username = channel.can_set_username;
                    f.need_save_to_database = true;
                }
                {
                    let photo = crate::photo::get_photo(
                        std::mem::take(&mut channel.chat_photo).unwrap_or_default(),
                    );
                    // on_update_channel_photo(c, channel_id, as_dialog_photo(
                    // ..., c->access_hash, photo, false), false) — the
                    // channel-photo diff machine; note the access hash arg.
                    self.on_update_channel_full_photo(env, channel_id, photo);
                }
                self.on_update_channel_full_invite_link(
                    env,
                    channel_id,
                    // the C++ `std::move(channel->exported_invite_)`
                    std::mem::take(&mut channel.exported_invite),
                );
                // read inbox/outbox, unread counts, pinned message, folder,
                // scheduled, ttl, group call, default send-as — the M.6 stubs.
                let mut bot_user_ids = Vec::new();
                for bot_info in &channel.bot_info {
                    let user_id = UserId::new(bot_info.user_id);
                    if !user_manager.is_user_bot_of(user_id) {
                        continue;
                    }
                    bot_user_ids.push(user_id);
                }
                self.on_update_channel_full_bot_user_ids(channel_id, bot_user_ids);
                let bot_commands =
                    user_manager.get_bot_commands(env, std::mem::take(&mut channel.bot_info), None);
                if let Some(f) = self.get_channel_full_mut(channel_id)
                    && f.bot_commands != bot_commands
                {
                    f.bot_commands = bot_commands;
                    f.is_changed = true;
                }
                self.on_update_channel_full_guard_bot_user_id(
                    user_manager,
                    channel_id,
                    UserId::new(channel.guard_bot_id),
                );
                let mut monoforum_channel_id = self
                    .get_channel(channel_id)
                    .map(|c| c.monoforum_channel_id)
                    .unwrap_or_default();
                if monoforum_channel_id.is_valid() {
                    let monoforum_channel = self.get_channel_force(
                        user_manager,
                        env,
                        monoforum_channel_id,
                        "ChannelFull",
                        false,
                    );
                    let reset = match &monoforum_channel {
                        None => true,
                        Some(mc) if is_megagroup => !mc.is_megagroup,
                        Some(mc) => mc.is_monoforum,
                    };
                    if reset {
                        tdutils::log_error!(
                            "Failed to add a monoforum link between {channel_id:?} and {monoforum_channel_id:?}"
                        );
                        monoforum_channel_id = ChannelId::default();
                    }
                }
                self.on_update_channel_full_monoforum_channel_id(
                    user_manager,
                    env,
                    channel_id,
                    monoforum_channel_id,
                );
                let linked_community_id = self
                    .get_channel(channel_id)
                    .map(|c| c.linked_community_id)
                    .unwrap_or_default();
                self.on_update_channel_full_linked_community_id(channel_id, linked_community_id);
                let mut linked_channel_id = ChannelId::default();
                if channel.flags & (1 << 14) != 0 {
                    linked_channel_id = ChannelId::new(channel.linked_chat_id);
                    let linked_channel = self.get_channel_force(
                        user_manager,
                        env,
                        linked_channel_id,
                        "ChannelFull",
                        false,
                    );
                    let reject = match &linked_channel {
                        None => true,
                        Some(lc) => {
                            is_megagroup == lc.is_megagroup || channel_id == linked_channel_id
                        }
                    };
                    if reject {
                        tdutils::log_error!(
                            "Failed to add a link between {channel_id:?} and {linked_channel_id:?}"
                        );
                        linked_channel_id = ChannelId::default();
                    }
                }
                self.on_update_channel_full_linked_channel_id(
                    user_manager,
                    env,
                    channel_id,
                    linked_channel_id,
                );
                {
                    let location = DialogLocation::from_telegram(
                        std::mem::take(&mut channel.location).unwrap_or_default(),
                    );
                    self.on_update_channel_full_location(user_manager, env, channel_id, location);
                }
                if is_megagroup {
                    self.on_update_channel_full_slow_mode_delay(
                        user_manager,
                        env,
                        channel_id,
                        channel.slowmode_seconds,
                        channel.slowmode_next_send_date,
                    );
                }
                // can_be_deleted sync, both directions, both save-only
                {
                    let full_can_be_deleted = channel.can_delete_channel;
                    if let Some(f) = self.get_channel_full_mut(channel_id)
                        && f.can_be_deleted != full_can_be_deleted
                    {
                        f.can_be_deleted = full_can_be_deleted;
                        f.need_save_to_database = true;
                    }
                    let full_can_be_deleted = self
                        .get_channel_full_const(channel_id)
                        .map(|f| f.can_be_deleted)
                        .unwrap_or(false);
                    if let Some(c) = self.get_channel_mut(channel_id)
                        && c.can_be_deleted != full_can_be_deleted
                    {
                        c.can_be_deleted = full_can_be_deleted;
                        c.need_save_to_database = true;
                    }
                }
                // the migration pair
                let mut migrated_from_chat_id = ChatId::new(channel.migrated_from_chat_id);
                let mut migrated_from_max_message_id =
                    MessageId::new(i64::from(channel.migrated_from_max_id));
                if channel.flags & (1 << 4) != 0
                    && (!migrated_from_chat_id.is_valid() && migrated_from_chat_id != ChatId::default()
                        // a non-zero id that fails the MessageId
                        // classification is reset (the C++ is_valid)
                        || !migrated_from_max_message_id.is_valid()
                            && migrated_from_max_message_id != MessageId::default())
                {
                    tdutils::log_error!(
                        "Receive migrate from {migrated_from_max_message_id:?} from {migrated_from_chat_id:?}"
                    );
                    migrated_from_chat_id = ChatId::default();
                    migrated_from_max_message_id = MessageId::default();
                }
                if let Some(f) = self.get_channel_full_mut(channel_id)
                    && (f.migrated_from_chat_id != migrated_from_chat_id
                        || f.migrated_from_max_message_id != migrated_from_max_message_id)
                {
                    f.migrated_from_chat_id = migrated_from_chat_id;
                    f.migrated_from_max_message_id = migrated_from_max_message_id;
                    f.is_changed = true;
                }
                if self.get_channel(channel_id).is_some_and(|c| c.is_changed) {
                    tdutils::log_error!(
                        "Receive inconsistent chatPhoto and chatPhotoInfo for {channel_id:?}"
                    );
                    self.update_channel(user_manager, env, channel_id, false, false);
                }
                if let Some(f) = self.get_channel_full_mut(channel_id) {
                    f.is_update_channel_full_sent = true;
                }
                self.update_channel_full(
                    user_manager,
                    env,
                    channel_id,
                    "on_get_channel_full",
                    false,
                );
                // the reverse monoforum link: the C++ dereferences the
                // force-loaded full before the null check; Rust guards first
                if monoforum_channel_id.is_valid()
                    && self.have_channel(monoforum_channel_id)
                    && !self.get_channel(channel_id).is_some_and(|c| c.is_monoforum)
                    && self
                        .get_channel_full_force(
                            user_manager,
                            env,
                            monoforum_channel_id,
                            true,
                            "on_get_channel_full",
                            false,
                        )
                        .is_some()
                {
                    self.on_update_channel_full_monoforum_channel_id(
                        user_manager,
                        env,
                        monoforum_channel_id,
                        channel_id,
                    );
                    self.update_channel_full(
                        user_manager,
                        env,
                        monoforum_channel_id,
                        "on_get_channel_full 2",
                        false,
                    );
                }
                // the reverse linked link
                if linked_channel_id.is_valid() && self.have_channel(linked_channel_id) {
                    self.on_update_channel_full_linked_channel_id(
                        user_manager,
                        env,
                        linked_channel_id,
                        channel_id,
                    );
                    self.update_channel_full(
                        user_manager,
                        env,
                        linked_channel_id,
                        "on_get_channel_full 2",
                        false,
                    );
                }
                // td_->suggested_action_manager_->set_dialog_pending_suggestions
                // — the wave-2/3 stub (flags.25).
                if monoforum_channel_id.is_valid()
                    && self.get_channel(channel_id).is_some_and(|c| c.is_monoforum)
                    && self.get_channel_full_const(monoforum_channel_id).is_none()
                {
                    // the C++ passes the promise through a pass-through
                    // lambda; with net blocked the promise errors directly
                    return self.reload_channel_full(
                        env,
                        monoforum_channel_id,
                        promise,
                        "on_get_channel_full 3",
                    );
                }
            }
            telegram_api::ChatFull::communityFull(_) => {}
        }
        promise.set_value(());
    }

    /// `on_update_chat_photo` (ChatManager.cpp:7446-7452) — the
    /// `telegram_api::ChatPhoto` wrapper.
    fn on_update_chat_photo(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        chat_id: ChatId,
        chat_photo: telegram_api::ChatPhoto,
    ) {
        let photo = DialogPhoto::get_dialog_photo(Some(chat_photo));
        self.on_update_chat_photo_impl(user_manager, env, chat_id, photo, true);
    }

    /// `on_update_chat_photo_impl` (ChatManager.cpp:7454-7480).
    fn on_update_chat_photo_impl(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        chat_id: ChatId,
        photo: DialogPhoto,
        invalidate_photo_cache: bool,
    ) {
        let mut photo = photo;
        if env.auth_is_bot {
            photo.minithumbnail = String::new();
        }
        let photo_differs = {
            let Some(c) = self.get_chat(chat_id) else {
                return;
            };
            Self::need_update_dialog_photo(&c.photo, &photo)
        };
        if photo_differs {
            tdutils::log_debug!("Update photo of {chat_id:?}");
            let small_file_id_valid;
            {
                let c = self.get_chat_mut(chat_id).expect("checked above");
                c.photo = photo;
                c.is_photo_changed = true;
                c.need_save_to_database = true;
                small_file_id_valid = c.photo.small_file_id.is_valid();
            }
            if invalidate_photo_cache && self.get_chat_full(chat_id).is_some() {
                if let Some(f) = self.get_chat_full_mut(chat_id)
                    && !f.photo.is_empty()
                {
                    f.photo = Photo::default();
                    f.is_changed = true;
                }
                if small_file_id_valid {
                    self.reload_chat_full(env, chat_id, Promise::default(), "on_update_chat_photo");
                }
                self.update_chat_full(user_manager, env, chat_id, "on_update_chat_photo", false);
            }
        } else {
            let Some(c) = self.get_chat_mut(chat_id) else {
                return;
            };
            if c.photo.minithumbnail != photo.minithumbnail {
                c.photo.minithumbnail = photo.minithumbnail;
                c.is_photo_changed = true;
                c.need_save_to_database = true;
            }
        }
    }

    // ---- M.5.28/M.5.29: the updateChat* intake family ----

    /// `on_update_chat_full_participants_short` (ChatManager.cpp:7527-7546)
    /// — the single-step version gate. True when the caller may apply.
    #[allow(dead_code)] // production caller: UpdatesManager (M.7) / M.5.47
    fn on_update_chat_full_participants_short(
        &mut self,
        env: &mut UserEnv,
        chat_id: ChatId,
        version: i32,
    ) -> bool {
        if version <= -1 {
            tdutils::log_error!("Receive wrong version {version} for {chat_id:?}");
            return false;
        }
        let Some(f) = self.chats_full.get_mut(&chat_id.get()) else {
            return false;
        };
        if f.version == -1 {
            tdutils::log_info!("{chat_id:?} chat members are unknown, nothing to update");
            return false;
        }
        if f.version + 1 == version {
            f.version = version;
            return true;
        }
        tdutils::log_info!(
            "Number of members in {chat_id:?} with version {} has changed, but new version is {version}",
            f.version
        );
        self.repair_chat_participants(env, chat_id);
        false
    }

    /// `on_update_chat_add_user` (ChatManager.cpp:6999-7068).
    #[allow(dead_code)] // production caller: UpdatesManager (M.7) / M.5.47
    #[allow(clippy::too_many_arguments)]
    fn on_update_chat_add_user(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        chat_id: ChatId,
        inviter_user_id: UserId,
        user_id: UserId,
        date: i32,
        version: i32,
    ) {
        if !chat_id.is_valid() {
            tdutils::log_error!("Receive invalid {chat_id:?}");
            return;
        }
        if !user_manager.have_min_user(user_id) {
            tdutils::log_error!("Can't find {user_id:?}");
            return;
        }
        if !user_manager.have_min_user(inviter_user_id) {
            tdutils::log_error!("Can't find {inviter_user_id:?}");
            return;
        }
        tdutils::log_info!(
            "Receive updateChatParticipantAdd to {chat_id:?} with {user_id:?} invited by {inviter_user_id:?} at {date} with version {version}"
        );
        if self
            .get_chat_full_force(user_manager, env, chat_id, "on_update_chat_add_user")
            .is_none()
        {
            tdutils::log_info!("Ignoring update about members of {chat_id:?}");
            return;
        }
        let (status_is_left, chat_version, participant_count) = {
            let Some(c) = self.get_chat(chat_id) else {
                tdutils::log_error!(
                    "Receive updateChatParticipantAdd for unknown {chat_id:?}. Couldn't apply it"
                );
                self.repair_chat_participants(env, chat_id);
                return;
            };
            (c.status.is_left(), c.version, c.participant_count)
        };
        if status_is_left {
            tdutils::log_warning!(
                "Receive updateChatParticipantAdd for left {chat_id:?}. Couldn't apply it"
            );
            self.repair_chat_participants(env, chat_id);
            return;
        }
        if self.on_update_chat_full_participants_short(env, chat_id, version) {
            let mut readded = false;
            let mut found_same_inviter = false;
            {
                let Some(f) = self.chats_full.get_mut(&chat_id.get()) else {
                    return;
                };
                let user_dialog_id = DialogId::from_user_id(user_id);
                if let Some(participant) = f
                    .participants
                    .iter_mut()
                    .find(|p| p.dialog_id_ == user_dialog_id)
                {
                    if participant.inviter_user_id_ != inviter_user_id {
                        tdutils::log_error!(
                            "{user_id:?} was re-added to {chat_id:?} by {inviter_user_id:?}, previously invited by {:?}",
                            participant.inviter_user_id_
                        );
                        participant.inviter_user_id_ = inviter_user_id;
                        participant.joined_date_ = date;
                        self.repair_chat_participants(env, chat_id);
                        readded = true;
                    } else {
                        tdutils::log_info!("{user_id:?} was re-added to {chat_id:?}");
                        found_same_inviter = true;
                    }
                    let _ = readded || found_same_inviter;
                } else {
                    let status = if user_id == f.creator_user_id {
                        DialogParticipantStatus::creator(true, false, String::new())
                    } else {
                        DialogParticipantStatus::member(0, String::new())
                    };
                    f.participants.push(DialogParticipant::new(
                        user_dialog_id,
                        inviter_user_id,
                        date,
                        status,
                    ));
                }
            }
            if readded || found_same_inviter {
                return;
            }
            self.update_chat_online_member_count_full(user_manager, env, chat_id, false);
            if let Some(f) = self.chats_full.get_mut(&chat_id.get()) {
                f.is_changed = true;
            }
            self.update_chat_full(user_manager, env, chat_id, "on_update_chat_add_user", false);
            if let Some(f) = self.get_chat_full(chat_id)
                && f.version == chat_version
                && f.participants.len() as i32 != participant_count
            {
                // chat is already updated
                tdutils::log_error!(
                    "Number of members in {chat_id:?} with version {} is {} but there are {} members in the ChatFull",
                    f.version,
                    participant_count,
                    f.participants.len()
                );
                self.repair_chat_participants(env, chat_id);
            }
        }
    }

    /// `on_update_chat_edit_administrator` (ChatManager.cpp:7069-7138).
    #[allow(dead_code)] // production caller: UpdatesManager (M.7) / M.5.47
    fn on_update_chat_edit_administrator(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        chat_id: ChatId,
        user_id: UserId,
        is_administrator: bool,
        version: i32,
    ) {
        if !chat_id.is_valid() {
            tdutils::log_error!("Receive invalid {chat_id:?}");
            return;
        }
        if !user_manager.have_min_user(user_id) {
            tdutils::log_error!("Can't find {user_id:?}");
            return;
        }
        tdutils::log_info!(
            "Receive updateChatParticipantAdmin in {chat_id:?} with {user_id:?}, administrator rights {is_administrator} with version {version}"
        );
        let Some(c) = self.get_chat_force(
            user_manager,
            env,
            chat_id,
            "on_update_chat_edit_administrator",
        ) else {
            tdutils::log_info!("Ignoring update about members of unknown {chat_id:?}");
            return;
        };
        if c.status.is_left() {
            tdutils::log_warning!(
                "Receive updateChatParticipantAdmin for left {chat_id:?}. Couldn't apply it"
            );
            self.repair_chat_participants(env, chat_id);
            return;
        }
        if version <= -1 {
            tdutils::log_error!("Receive wrong version {version} for {chat_id:?}");
            return;
        }
        debug_assert!(c.version >= 0);
        let status = if is_administrator {
            DialogParticipantStatus::group_administrator(c.status.is_creator(), String::new())
        } else {
            DialogParticipantStatus::member(0, String::new())
        };
        let mut apply_status = false;
        if version > c.version {
            if version != c.version + 1 {
                tdutils::log_info!(
                    "Administrators of {chat_id:?} with version {} has changed, but new version is {version}",
                    c.version
                );
                self.repair_chat_participants(env, chat_id);
                return;
            }
            c.version = version;
            c.need_save_to_database = true;
            // if chat with version was already received, the update is
            // already processed; call on_update_chat_status only if the
            // version advanced
            apply_status = user_id == user_manager.get_my_id() && !c.status.is_creator();
        }
        if apply_status {
            self.on_update_chat_status(user_manager, env, chat_id, status.clone());
        }
        self.update_chat(user_manager, env, chat_id, false, false);
        if self
            .get_chat_full_force(
                user_manager,
                env,
                chat_id,
                "on_update_chat_edit_administrator",
            )
            .is_some()
        {
            let applied = {
                let Some(f) = self.chats_full.get_mut(&chat_id.get()) else {
                    return;
                };
                if f.version + 1 == version {
                    let user_dialog_id = DialogId::from_user_id(user_id);
                    if let Some(participant) = f
                        .participants
                        .iter_mut()
                        .find(|p| p.dialog_id_ == user_dialog_id)
                    {
                        let rank = participant.status_.get_rank().to_owned();
                        let mut new_status = status;
                        new_status.set_rank(rank);
                        participant.status_ = new_status;
                        f.is_changed = true;
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            };
            if applied {
                self.update_chat_full(
                    user_manager,
                    env,
                    chat_id,
                    "on_update_chat_edit_administrator",
                    false,
                );
                return;
            }
            tdutils::log_info!("Can't find chat member or version have increased too much");
            self.repair_chat_participants(env, chat_id);
        }
    }

    /// `on_update_chat_participant_rank` (ChatManager.cpp:7140-7201).
    #[allow(dead_code)] // production caller: UpdatesManager (M.7) / M.5.47
    fn on_update_chat_participant_rank(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        chat_id: ChatId,
        user_id: UserId,
        rank: String,
        version: i32,
    ) {
        if !chat_id.is_valid() {
            tdutils::log_error!("Receive invalid {chat_id:?}");
            return;
        }
        if !user_manager.have_min_user(user_id) {
            tdutils::log_error!("Can't find {user_id:?}");
            return;
        }
        tdutils::log_info!(
            "Receive updateChatParticipantRank in {chat_id:?} with {user_id:?}, tag {rank} with version {version}"
        );
        let Some(c) = self.get_chat_force(
            user_manager,
            env,
            chat_id,
            "on_update_chat_participant_rank",
        ) else {
            tdutils::log_info!("Ignoring update about members of unknown {chat_id:?}");
            return;
        };
        if c.status.is_left() {
            tdutils::log_warning!(
                "Receive updateChatParticipantRank for left {chat_id:?}. Couldn't apply it"
            );
            self.repair_chat_participants(env, chat_id);
            return;
        }
        if version <= -1 {
            tdutils::log_error!("Receive wrong version {version} for {chat_id:?}");
            return;
        }
        debug_assert!(c.version >= 0);
        if version > c.version {
            if version != c.version + 1 {
                tdutils::log_info!(
                    "Members of {chat_id:?} with version {} has changed, but new version is {version}",
                    c.version
                );
                self.repair_chat_participants(env, chat_id);
                return;
            }
            c.version = version;
            c.need_save_to_database = true;
        }
        self.update_chat(user_manager, env, chat_id, false, false);
        if self
            .get_chat_full_force(
                user_manager,
                env,
                chat_id,
                "on_update_chat_participant_rank",
            )
            .is_some()
        {
            let mut applied = false;
            if let Some(f) = self.chats_full.get_mut(&chat_id.get())
                && f.version + 1 == version
            {
                let user_dialog_id = DialogId::from_user_id(user_id);
                if let Some(participant) = f
                    .participants
                    .iter_mut()
                    .find(|p| p.dialog_id_ == user_dialog_id)
                    && participant.status_.set_rank(rank.clone())
                {
                    f.is_changed = true;
                    applied = true;
                }
            }
            if applied {
                self.update_chat_full(
                    user_manager,
                    env,
                    chat_id,
                    "on_update_chat_participant_rank",
                    false,
                );
                return;
            }
            tdutils::log_info!("Can't find chat member or version have increased too much");
            self.repair_chat_participants(env, chat_id);
        }
    }

    /// `on_update_chat_delete_user` (ChatManager.cpp:7203-7256).
    #[allow(dead_code)] // production caller: UpdatesManager (M.7) / M.5.47
    fn on_update_chat_delete_user(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        chat_id: ChatId,
        user_id: UserId,
        version: i32,
    ) {
        if !chat_id.is_valid() {
            tdutils::log_error!("Receive invalid {chat_id:?}");
            return;
        }
        if !user_manager.have_min_user(user_id) {
            tdutils::log_error!("Can't find {user_id:?}");
            return;
        }
        tdutils::log_info!(
            "Receive updateChatParticipantDelete from {chat_id:?} with {user_id:?} and version {version}"
        );
        if self
            .get_chat_full_force(user_manager, env, chat_id, "on_update_chat_delete_user")
            .is_none()
        {
            tdutils::log_info!("Ignoring update about members of {chat_id:?}");
            return;
        }
        let Some(c) = self.get_chat_force(user_manager, env, chat_id, "on_update_chat_delete_user")
        else {
            tdutils::log_error!("Receive updateChatParticipantDelete for unknown {chat_id:?}");
            self.repair_chat_participants(env, chat_id);
            return;
        };
        if user_id == user_manager.get_my_id() {
            if c.status.is_member() {
                tdutils::log_warning!(
                    "User was removed from {chat_id:?} but it is not left the group. Possible if updates comes out of order"
                );
            }
            return;
        }
        if c.status.is_left() {
            tdutils::log_info!("Receive updateChatParticipantDelete for left {chat_id:?}");
            self.repair_chat_participants(env, chat_id);
            return;
        }
        if !self.on_update_chat_full_participants_short(env, chat_id, version) {
            return;
        }
        let removed = {
            let Some(f) = self.chats_full.get_mut(&chat_id.get()) else {
                return;
            };
            let user_dialog_id = DialogId::from_user_id(user_id);
            match f
                .participants
                .iter()
                .position(|p| p.dialog_id_ == user_dialog_id)
            {
                Some(position) => {
                    // the C++ swap-remove
                    f.participants[position] = f.participants[f.participants.len() - 1].clone();
                    f.participants.pop();
                    f.is_changed = true;
                    true
                }
                None => {
                    tdutils::log_error!(
                        "Can't find basic group member {user_id:?} in {chat_id:?} to be removed"
                    );
                    false
                }
            }
        };
        if !removed {
            self.repair_chat_participants(env, chat_id);
            return;
        }
        self.update_chat_online_member_count_full(user_manager, env, chat_id, false);
        self.update_chat_full(
            user_manager,
            env,
            chat_id,
            "on_update_chat_delete_user",
            false,
        );
        let participant_count = self.get_chat(chat_id).map(|c| c.participant_count);
        if let (Some(f), Some(pc)) = (self.get_chat_full(chat_id), participant_count)
            && f.participants.len() as i32 != pc
        {
            self.repair_chat_participants(env, chat_id);
        }
    }

    /// `on_update_chat_pinned_message` (ChatManager.cpp:7366-7409).
    #[allow(dead_code)] // production caller: UpdatesManager (M.7) / M.5.47
    fn on_update_chat_pinned_message(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        chat_id: ChatId,
        pinned_message_id: MessageId,
        version: i32,
    ) {
        if !chat_id.is_valid() {
            tdutils::log_error!("Receive invalid {chat_id:?}");
            return;
        }
        let Some(c) =
            self.get_chat_force(user_manager, env, chat_id, "on_update_chat_pinned_message")
        else {
            tdutils::log_info!("Ignoring update about unknown {chat_id:?}");
            return;
        };
        tdutils::log_info!(
            "Receive updateChatPinnedMessage in {chat_id:?} with {pinned_message_id:?} and version {version}. Current version is {}/{}",
            c.version,
            c.pinned_message_version
        );
        if c.status.is_left() {
            // repair 'just in case'
            self.repair_chat_participants(env, chat_id);
            return;
        }
        if version <= -1 {
            tdutils::log_error!("Receive wrong version {version} for {chat_id:?}");
            return;
        }
        debug_assert!(c.version >= 0);
        if version >= c.pinned_message_version {
            if version != c.version + 1 && version != c.version {
                tdutils::log_info!(
                    "Pinned message of {chat_id:?} with version {} has changed, but new version is {version}",
                    c.version
                );
                self.repair_chat_participants(env, chat_id);
            } else if version == c.version + 1
                && let Some(c) = self.get_chat_mut(chat_id)
            {
                c.version = version;
                c.need_save_to_database = true;
            }
            // messages_manager on_update_dialog_last_pinned_message_id — the
            // M.10 stub (runs on both sub-branches).
            if let Some(c) = self.get_chat_mut(chat_id)
                && version > c.pinned_message_version
            {
                tdutils::log_info!(
                    "Change pinned message version of {chat_id:?} from {} to {version}",
                    c.pinned_message_version
                );
                c.pinned_message_version = version;
                c.need_save_to_database = true;
            }
            self.update_chat(user_manager, env, chat_id, false, false);
        }
    }

    /// `on_update_chat_description` (ChatManager.cpp:7509-7526).
    #[allow(dead_code)] // production caller: UpdatesManager (M.7) / M.5.47
    fn on_update_chat_description(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        chat_id: ChatId,
        description: String,
    ) {
        if !chat_id.is_valid() {
            tdutils::log_error!("Receive invalid {chat_id:?}");
            return;
        }
        if self
            .get_chat_full_force(user_manager, env, chat_id, "on_update_chat_description")
            .is_none()
        {
            return;
        }
        let mut changed = false;
        if let Some(f) = self.get_chat_full_mut(chat_id)
            && f.description != description
        {
            f.description = description;
            f.is_changed = true;
            changed = true;
        }
        if changed {
            // td_->group_call_manager_->on_update_dialog_about — the wave-2
            // stub.
            self.update_chat_full(
                user_manager,
                env,
                chat_id,
                "on_update_chat_description",
                false,
            );
        }
    }

    /// `on_update_chat_bot_commands` (ChatManager.cpp:7602-7607).
    #[allow(dead_code)] // production caller: UpdatesManager (M.7) / M.5.47
    fn on_update_chat_bot_commands(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        chat_id: ChatId,
        bot_commands: BotCommands,
    ) {
        if self
            .get_chat_full_force(user_manager, env, chat_id, "on_update_chat_bot_commands")
            .is_none()
        {
            return;
        }
        let mut changed = false;
        if let Some(f) = self.get_chat_full_mut(chat_id)
            && BotCommands::update_all_bot_commands(&mut f.bot_commands, bot_commands)
        {
            f.is_changed = true;
            changed = true;
        }
        if changed {
            self.update_chat_full(
                user_manager,
                env,
                chat_id,
                "on_update_chat_bot_commands",
                false,
            );
        }
    }

    /// `on_update_chat_permanent_invite_link` (ChatManager.cpp:7610-7614).
    #[allow(dead_code)] // production caller: UpdatesManager (M.7) / M.5.47
    fn on_update_chat_permanent_invite_link(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        chat_id: ChatId,
        invite_link: &DialogInviteLink,
    ) {
        if self
            .get_chat_full_force(
                user_manager,
                env,
                chat_id,
                "on_update_chat_permanent_invite_link",
            )
            .is_none()
        {
            return;
        }
        let mut changed = false;
        if let Some(f) = self.get_chat_full_mut(chat_id)
            && Self::update_permanent_invite_link(&mut f.invite_link, invite_link.clone())
        {
            f.is_changed = true;
            changed = true;
        }
        if changed {
            self.update_chat_full(
                user_manager,
                env,
                chat_id,
                "on_update_chat_permanent_invite_link",
                false,
            );
        }
    }

    /// `on_get_chat_empty` (ChatManager.cpp:9152-9162) — never changes
    /// state.
    fn on_get_chat_empty(&mut self, chat: telegram_api::chatEmpty, source: &str) {
        let chat_id = ChatId::new(chat.id);
        if !chat_id.is_valid() {
            tdutils::log_error!("Receive invalid {chat_id:?} from {source}");
            return;
        }
        if !self.have_chat(chat_id) {
            tdutils::log_error!(
                "Have no information about {chat_id:?} but received chatEmpty from {source}"
            );
        }
    }

    /// `on_get_chat` — the basic-group constructor
    /// (ChatManager.cpp:9164-9252).
    fn on_get_basic_group_chat(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        chat: telegram_api::chat,
        source: &str,
    ) {
        let mut chat = chat;
        let debug_str = format!(" from {source}");
        let chat_id = ChatId::new(chat.id);
        if !chat_id.is_valid() {
            tdutils::log_error!("Receive invalid {chat_id:?}{debug_str}");
            return;
        }

        // the status ladder: creator, then admin, then left, then member
        let status = if chat.creator {
            DialogParticipantStatus::creator(!chat.left, false, String::new())
        } else if chat.flags & (1 << 13) != 0 {
            DialogParticipantStatus::administrator(
                AdministratorRights::from_chat_admin_rights(
                    chat.admin_rights.as_ref(),
                    ChannelType::Unknown,
                ),
                String::new(),
                false,
            )
        } else if chat.left {
            DialogParticipantStatus::left()
        } else {
            DialogParticipantStatus::member(0, String::new())
        };

        // the upgraded-to supergroup: unknown channels are created on the
        // spot so the client sees every added channel
        let mut migrated_to_channel_id = ChannelId::default();
        match &chat.migrated_to {
            Some(telegram_api::InputChannel::inputChannel(ic)) => {
                migrated_to_channel_id = ChannelId::new(ic.channel_id);
                if !self.have_channel_force(user_manager, env, migrated_to_channel_id, source) {
                    if !migrated_to_channel_id.is_valid() {
                        tdutils::log_error!(
                            "Receive invalid {migrated_to_channel_id:?}{debug_str}"
                        );
                    } else {
                        {
                            let c = self.add_channel(migrated_to_channel_id);
                            c.access_hash = ic.access_hash;
                            c.title = chat.title.clone();
                            c.status = DialogParticipantStatus::left();
                            c.is_megagroup = true;
                        }
                        // we definitely need to call update_channel, because
                        // the client should know about every added channel
                        self.update_channel(
                            user_manager,
                            env,
                            migrated_to_channel_id,
                            false,
                            false,
                        );
                        // get_channel_queries_.add_query — [blocked: net]
                        // (M.5.44).
                    }
                }
            }
            Some(telegram_api::InputChannel::inputChannelEmpty(_))
            | Some(telegram_api::InputChannel::inputChannelFromMessage(_)) => {
                tdutils::log_error!(
                    "Receive invalid information about upgraded supergroup for {chat_id:?}{debug_str}"
                );
            }
            // the C++ `chat.migrated_to_ != nullptr` guard
            None => {}
        }

        // get_chat_force first, to load the versions from the database
        let c_exists = self
            .get_chat_force(user_manager, env, chat_id, source)
            .is_some();
        if !c_exists {
            self.add_chat(chat_id);
        }

        Self::on_update_chat_title(
            self.get_chat_mut(chat_id).expect("added above"),
            chat_id,
            std::mem::take(&mut chat.title),
        );
        if !status.is_left() {
            self.on_update_chat_participant_count(
                env,
                chat_id,
                chat.participants_count,
                chat.version,
                &debug_str,
            );
        } else {
            chat.photo = Some(telegram_api::ChatPhoto::chatPhotoEmpty(
                telegram_api::chatPhotoEmpty {},
            ));
        }

        {
            let c = self.get_chat_mut(chat_id).expect("added above");
            if c.date != chat.date {
                if c.date != 0 {
                    tdutils::log_error!(
                        "Chat creation date has changed from {} to {}{debug_str}",
                        c.date,
                        chat.date
                    );
                }
                c.date = chat.date;
                c.need_save_to_database = true;
            }
        }

        self.on_update_chat_status(user_manager, env, chat_id, status);
        Self::on_update_chat_default_permissions(
            self.get_chat_mut(chat_id).expect("added above"),
            chat_id,
            RestrictedRights::from_chat_banned_rights(
                chat.default_banned_rights.as_ref(),
                ChannelType::Unknown,
            ),
            chat.version,
        );
        let chat_photo = std::mem::take(&mut chat.photo).unwrap_or_default();
        self.on_update_chat_photo(user_manager, env, chat_id, chat_photo);
        Self::on_update_chat_active(
            self.get_chat_mut(chat_id).expect("added above"),
            chat_id,
            !chat.deactivated,
        );
        Self::on_update_chat_noforwards(
            self.get_chat_mut(chat_id).expect("added above"),
            chat_id,
            chat.noforwards,
        );
        Self::on_update_chat_migrated_to_channel_id(
            self.get_chat_mut(chat_id).expect("added above"),
            chat_id,
            migrated_to_channel_id,
        );

        let migrated_valid = migrated_to_channel_id.is_valid();
        {
            let c = self.get_chat_mut(chat_id).expect("added above");
            if !c.is_active && !migrated_valid {
                tdutils::log_info!("{chat_id:?} is deactivated{debug_str}");
            }
            if c.cache_version != Chat::CACHE_VERSION {
                c.cache_version = Chat::CACHE_VERSION;
                c.need_save_to_database = true;
            }
            c.is_received_from_server = true;
        }
        self.update_chat(user_manager, env, chat_id, false, false);
        // td_->messages_manager_->on_update_dialog_group_call — the M.10
        // stub (GroupCallManager slice).
    }

    /// `on_get_chat_forbidden` (ChatManager.cpp:9254-9283).
    fn on_get_chat_forbidden(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        chat: telegram_api::chatForbidden,
        source: &str,
    ) {
        let chat_id = ChatId::new(chat.id);
        if !chat_id.is_valid() {
            tdutils::log_error!("Receive invalid {chat_id:?} from {source}");
            return;
        }
        let is_uninited = self
            .get_chat_force(user_manager, env, chat_id, source)
            .is_none();
        self.add_chat(chat_id);
        Self::on_update_chat_title(
            self.get_chat_mut(chat_id).expect("added above"),
            chat_id,
            chat.title,
        );
        self.on_update_chat_photo(
            user_manager,
            env,
            chat_id,
            telegram_api::ChatPhoto::chatPhotoEmpty(telegram_api::chatPhotoEmpty {}),
        );
        {
            let c = self.get_chat_mut(chat_id).expect("added above");
            if c.date != 0 {
                // removed in 38-th layer
                c.date = 0;
                c.need_save_to_database = true;
            }
        }
        self.on_update_chat_status(
            user_manager,
            env,
            chat_id,
            DialogParticipantStatus::banned(0, String::new()),
        );
        if is_uninited {
            Self::on_update_chat_active(
                self.get_chat_mut(chat_id).expect("added above"),
                chat_id,
                true,
            );
            Self::on_update_chat_migrated_to_channel_id(
                self.get_chat_mut(chat_id).expect("added above"),
                chat_id,
                ChannelId::default(),
            );
        }
        {
            let c = self.get_chat_mut(chat_id).expect("added above");
            if c.cache_version != Chat::CACHE_VERSION {
                c.cache_version = Chat::CACHE_VERSION;
                c.need_save_to_database = true;
            }
            c.is_received_from_server = true;
        }
        self.update_chat(user_manager, env, chat_id, false, false);
    }

    /// `on_get_chat` (ChatManager.cpp:5704-5733) — the constructor
    /// dispatcher. Communities are consumed and dropped by the
    /// CommunityManager stub.
    pub(crate) fn on_get_chat(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        chat: telegram_api::Chat,
        source: &str,
    ) {
        match chat {
            telegram_api::Chat::chatEmpty(c) => self.on_get_chat_empty(c, source),
            telegram_api::Chat::chat(c) => {
                self.on_get_basic_group_chat(user_manager, env, c, source)
            }
            telegram_api::Chat::chatForbidden(c) => {
                self.on_get_chat_forbidden(user_manager, env, c, source)
            }
            telegram_api::Chat::channel(c) => self.on_get_channel(user_manager, env, c, source),
            telegram_api::Chat::channelForbidden(c) => {
                self.on_get_channel_forbidden(user_manager, env, c, source)
            }
            // td_->community_manager_->on_get_community — consume and drop;
            // the community slice owns them.
            chat @ (telegram_api::Chat::community(_)
            | telegram_api::Chat::communityForbidden(_)) => {
                let _ = chat;
            }
        }
    }

    /// `on_get_chats` (ChatManager.cpp:5735-5757) — communities before
    /// channels before plain chats, because the basic-group intake
    /// consults the migrated-to channel.
    #[allow(dead_code)] // production caller: UpdatesManager (M.7) / M.5.47
    pub(crate) fn on_get_chats(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        chats: Vec<telegram_api::Chat>,
        source: &str,
    ) {
        let mut chats: Vec<Option<telegram_api::Chat>> = chats.into_iter().map(Some).collect();
        let take_matching =
            |pred: fn(&telegram_api::Chat) -> bool, chats: &mut Vec<Option<telegram_api::Chat>>| {
                chats
                    .iter_mut()
                    .filter(|c| c.as_ref().is_some_and(pred))
                    .map(|c| c.take().expect("filtered to Some"))
                    .collect::<Vec<_>>()
            };
        for chat in take_matching(
            |c| {
                matches!(
                    c,
                    telegram_api::Chat::community(_) | telegram_api::Chat::communityForbidden(_)
                )
            },
            &mut chats,
        ) {
            self.on_get_chat(user_manager, env, chat, source);
        }
        for chat in take_matching(
            |c| {
                matches!(
                    c,
                    telegram_api::Chat::channel(_) | telegram_api::Chat::channelForbidden(_)
                )
            },
            &mut chats,
        ) {
            self.on_get_chat(user_manager, env, chat, source);
        }
        for chat in take_matching(|_| true, &mut chats) {
            self.on_get_chat(user_manager, env, chat, source);
        }
    }

    /// `get_channel_ids` (ChatManager.cpp:4048-4062).
    #[allow(dead_code)] // production caller: MessagesManager intakes (M.10)
    pub(crate) fn get_channel_ids(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        chats: Vec<telegram_api::Chat>,
        source: &str,
    ) -> Vec<ChannelId> {
        let mut channel_ids = Vec::new();
        for chat in chats {
            let channel_id = Self::get_channel_id(&chat);
            if !channel_id.is_valid() {
                tdutils::log_error!("Receive invalid {channel_id:?} from {source}");
                continue;
            }
            self.on_get_chat(user_manager, env, chat, source);
            if self.have_channel(channel_id) {
                channel_ids.push(channel_id);
            }
        }
        channel_ids
    }

    // ---- M.5.21/M.5.30-M.5.32 helpers: the channel compare-then-set
    // family the intakes apply (ChatManager.cpp:7594-8311) ----

    /// `on_update_channel_title` (ChatManager.cpp:7702-7706).
    fn on_update_channel_title(c: &mut Channel, channel_id: ChannelId, title: String) {
        if c.title != title {
            c.title = title;
            c.is_title_changed = true;
            c.need_save_to_database = true;
        }
        let _ = channel_id;
    }

    /// `on_update_channel_usernames` (ChatManager.cpp:8037-8053) plus
    /// `on_channel_usernames_changed` (cpp:8055-8063).
    fn on_update_channel_usernames(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        channel_id: ChannelId,
        usernames: Usernames,
    ) {
        let differs = self
            .get_channel(channel_id)
            .is_some_and(|c| c.usernames != usernames);
        if differs {
            // td_->dialog_manager_->on_dialog_usernames_updated and
            // messages_manager_->on_dialog_usernames_updated — the M.10 stubs.
            let sent = self
                .get_channel(channel_id)
                .is_some_and(|c| c.is_update_supergroup_sent);
            if sent {
                let (old, new) = {
                    let c = self.get_channel_mut(channel_id).expect("checked above");
                    (c.usernames.clone(), usernames.clone())
                };
                self.on_channel_usernames_changed(user_manager, env, channel_id, &old, &new);
            }
            let c = self.get_channel_mut(channel_id).expect("checked above");
            c.usernames = usernames;
            c.is_username_changed = true;
            c.is_changed = true;
        } else {
            // td_->dialog_manager_->on_dialog_usernames_received — the M.10 stub.
        }
    }

    /// `on_channel_usernames_changed` (ChatManager.cpp:8055-8063).
    fn on_channel_usernames_changed(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        channel_id: ChannelId,
        old: &Usernames,
        new: &Usernames,
    ) {
        let have_channel_full = self.get_channel_full_const(channel_id).is_some();
        if !old.has_first_username() || !new.has_first_username() {
            let drop = !self
                .get_channel(channel_id)
                .is_some_and(|c| c.is_slow_mode_enabled);
            self.invalidate_channel_full(
                user_manager,
                env,
                channel_id,
                drop,
                "on_channel_usernames_changed",
            );
        }
        // must not load ChannelFull, because must not change the Channel
        debug_assert!(have_channel_full == self.get_channel_full_const(channel_id).is_some());
    }

    /// `on_update_channel_photo` (ChatManager.cpp:7594-7648) — the
    /// channel twin of [`Self::on_update_chat_photo_impl`].
    fn on_update_channel_photo(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        channel_id: ChannelId,
        chat_photo: telegram_api::ChatPhoto,
    ) {
        // the C++ passes c->access_hash to the file-manager form — dropped
        // by the Files-step stand-in
        let photo = DialogPhoto::get_dialog_photo(Some(chat_photo));
        let mut photo = photo;
        if env.auth_is_bot {
            photo.minithumbnail = String::new();
        }
        let photo_differs = {
            let Some(c) = self.get_channel(channel_id) else {
                return;
            };
            Self::need_update_dialog_photo(&c.photo, &photo)
        };
        if photo_differs {
            tdutils::log_debug!("Update photo of {channel_id:?}");
            let small_file_id_valid;
            {
                let c = self.get_channel_mut(channel_id).expect("checked above");
                c.photo = photo;
                c.is_photo_changed = true;
                c.need_save_to_database = true;
                small_file_id_valid = c.photo.small_file_id.is_valid();
            }
            if small_file_id_valid
                && let Some(f) = self.get_channel_full_mut(channel_id)
                && f.expires_at > 0.0
            {
                f.expires_at = 0.0;
                f.need_save_to_database = true;
            }
            if small_file_id_valid {
                self.reload_channel_full(
                    env,
                    channel_id,
                    Promise::default(),
                    "on_update_channel_photo",
                );
            }
            self.update_channel_full(
                user_manager,
                env,
                channel_id,
                "on_update_channel_photo",
                false,
            );
        } else {
            let Some(c) = self.get_channel_mut(channel_id) else {
                return;
            };
            if c.photo.minithumbnail != photo.minithumbnail {
                c.photo.minithumbnail = photo.minithumbnail;
                c.is_photo_changed = true;
                c.need_save_to_database = true;
            }
        }
    }

    /// `on_update_channel_emoji_status` (ChatManager.cpp:7651-7658).
    fn on_update_channel_emoji_status(
        c: &mut Channel,
        channel_id: ChannelId,
        emoji_status: Option<EmojiStatus>,
    ) {
        if c.emoji_status != emoji_status {
            tdutils::log_debug!("Update {channel_id:?} emoji status");
            c.emoji_status = emoji_status;
            c.is_emoji_status_changed = true;
            c.need_save_to_database = true;
        }
    }

    /// `on_update_channel_accent_color_id` (ChatManager.cpp:7661-7668).
    fn on_update_channel_accent_color_id(
        c: &mut Channel,
        channel_id: ChannelId,
        mut accent_color_id: AccentColorId,
    ) {
        if accent_color_id == AccentColorId::from_peer_id(channel_id.get())
            || !accent_color_id.is_valid()
        {
            accent_color_id = AccentColorId::default();
        }
        if c.accent_color_id != accent_color_id {
            c.accent_color_id = accent_color_id;
            c.is_accent_color_changed = true;
            c.need_save_to_database = true;
        }
    }

    /// `on_update_channel_background_custom_emoji_id`
    /// (ChatManager.cpp:7672-7677) — flips the accent-color flag.
    fn on_update_channel_background_custom_emoji_id(
        c: &mut Channel,
        background_custom_emoji_id: CustomEmojiId,
    ) {
        if c.background_custom_emoji_id != background_custom_emoji_id {
            c.background_custom_emoji_id = background_custom_emoji_id;
            c.is_accent_color_changed = true;
            c.need_save_to_database = true;
        }
    }

    /// `on_update_channel_profile_accent_color_id`
    /// (ChatManager.cpp:7681-7689).
    fn on_update_channel_profile_accent_color_id(
        c: &mut Channel,
        mut profile_accent_color_id: AccentColorId,
    ) {
        if !profile_accent_color_id.is_valid() {
            profile_accent_color_id = AccentColorId::default();
        }
        if c.profile_accent_color_id != profile_accent_color_id {
            c.profile_accent_color_id = profile_accent_color_id;
            c.is_accent_color_changed = true;
            c.need_save_to_database = true;
        }
    }

    /// `on_update_channel_profile_background_custom_emoji_id`
    /// (ChatManager.cpp:7693-7699) — flips the accent-color flag.
    fn on_update_channel_profile_background_custom_emoji_id(
        c: &mut Channel,
        profile_background_custom_emoji_id: CustomEmojiId,
    ) {
        if c.profile_background_custom_emoji_id != profile_background_custom_emoji_id {
            c.profile_background_custom_emoji_id = profile_background_custom_emoji_id;
            c.is_accent_color_changed = true;
            c.need_save_to_database = true;
        }
    }

    /// `on_update_channel_default_permissions`
    /// (ChatManager.cpp:7809-7816) — broadcast channels ignore them.
    fn on_update_channel_default_permissions(
        c: &mut Channel,
        channel_id: ChannelId,
        default_permissions: RestrictedRights,
    ) {
        if c.is_megagroup && c.default_permissions != default_permissions {
            tdutils::log_info!("{channel_id:?} default permissions changed");
            c.default_permissions = default_permissions;
            c.is_default_permissions_changed = true;
            c.need_save_to_database = true;
        }
    }

    /// `on_update_channel_has_location` (ChatManager.cpp:7820-7825).
    fn on_update_channel_has_location(c: &mut Channel, has_location: bool) {
        if c.has_location != has_location {
            c.has_location = has_location;
            c.is_has_location_changed = true;
            c.need_save_to_database = true;
        }
    }

    /// `on_update_channel_noforwards` (ChatManager.cpp:7829-7834).
    fn on_update_channel_noforwards(c: &mut Channel, noforwards: bool) {
        if c.noforwards != noforwards {
            c.noforwards = noforwards;
            c.is_noforwards_changed = true;
            c.need_save_to_database = true;
        }
    }

    /// `on_update_channel_is_forum` (ChatManager.cpp:7838-7848).
    fn on_update_channel_is_forum(c: &mut Channel, is_forum: bool, is_forum_tabs: bool) {
        if c.is_forum != is_forum || c.is_forum_tabs != is_forum_tabs {
            c.is_forum = is_forum;
            c.is_forum_tabs = is_forum_tabs;
            c.is_is_forum_changed = true;
            c.is_changed = true;
        }
    }

    /// `on_update_channel_stories_hidden` (ChatManager.cpp:7973-7982).
    fn on_update_channel_stories_hidden(env: &UserEnv, c: &mut Channel, stories_hidden: bool) {
        if env.auth_is_bot {
            return;
        }
        if c.stories_hidden != stories_hidden {
            c.stories_hidden = stories_hidden;
            c.is_stories_hidden_changed = true;
            c.need_save_to_database = true;
        }
    }

    /// `on_update_channel_bot_verification_icon`
    /// (ChatManager.cpp:7986-7990).
    fn on_update_channel_bot_verification_icon(
        c: &mut Channel,
        bot_verification_icon: CustomEmojiId,
    ) {
        if c.bot_verification_icon != bot_verification_icon {
            c.bot_verification_icon = bot_verification_icon;
            c.is_changed = true;
        }
    }

    /// `MAX_ACTIVE_STORY_ID_RELOAD_TIME` (ChatManager.h:694).
    const MAX_ACTIVE_STORY_ID_RELOAD_TIME: f64 = 3600.0;

    /// `get_channel_active_story_state` (ChatManager.cpp:9880-9883) — the
    /// local story-state comparator.
    fn get_channel_active_story_state(c: &Channel) -> (StoryId, StoryId, bool) {
        (c.max_active_story_id, c.max_read_story_id, c.has_live_story)
    }

    /// `need_poll_channel_active_stories` (ChatManager.cpp:9875-9878).
    fn need_poll_channel_active_stories(env: &UserEnv, c: &Channel, channel_id: ChannelId) -> bool {
        if c.status.is_member() {
            return false;
        }
        // && have_input_peer_channel(c, channel_id, AccessRights::Read) —
        // the poll gate widens when the full input-peer access check lands
        let _ = (env, channel_id);
        true
    }

    /// `on_update_channel_story_ids_impl` (ChatManager.cpp:7865-7924).
    fn on_update_channel_story_ids_impl(
        &mut self,
        env: &mut UserEnv,
        channel_id: ChannelId,
        recent_story: telegram_api::RecentStory,
        max_read_story_id: StoryId,
    ) {
        if env.auth_is_bot {
            return;
        }
        let max_active_story_id = StoryId::new(recent_story.max_id);
        let has_live_story = recent_story.live;
        if max_active_story_id != StoryId::default() && !max_active_story_id.is_server() {
            tdutils::log_error!("Receive max active {max_active_story_id:?} for {channel_id:?}");
            return;
        }
        if max_read_story_id != StoryId::default() && !max_read_story_id.is_server() {
            tdutils::log_error!("Receive max read {max_read_story_id:?} for {channel_id:?}");
            return;
        }
        if has_live_story && max_active_story_id == StoryId::default() {
            tdutils::log_error!("Receive live story without identifier for {channel_id:?}");
            return;
        }
        let Some(c) = self.get_channel_mut(channel_id) else {
            return;
        };
        let active_story_state = Self::get_channel_active_story_state(c);
        if c.has_live_story != has_live_story {
            c.has_live_story = has_live_story;
            c.need_save_to_database = true;
        }
        if c.max_active_story_id != max_active_story_id {
            tdutils::log_info!("Change last active story in {channel_id:?}");
            c.max_active_story_id = max_active_story_id;
            c.need_save_to_database = true;
        }
        if Self::need_poll_channel_active_stories(env, c, channel_id) {
            let next = env.global.server_time() + Self::MAX_ACTIVE_STORY_ID_RELOAD_TIME;
            if next
                > c.max_active_story_id_next_reload_time
                    + Self::MAX_ACTIVE_STORY_ID_RELOAD_TIME / 5.0
            {
                c.max_active_story_id_next_reload_time = next;
                c.need_save_to_database = true;
            }
        }
        if !max_active_story_id.is_valid() {
            debug_assert!(max_read_story_id == StoryId::default());
            if c.max_read_story_id != StoryId::default() {
                c.max_read_story_id = StoryId::default();
                c.need_save_to_database = true;
            }
        } else if max_read_story_id.get() > c.max_read_story_id.get() {
            c.max_read_story_id = max_read_story_id;
            c.need_save_to_database = true;
        }
        if active_story_state != Self::get_channel_active_story_state(c) {
            c.is_changed = true;
        }
    }

    /// `on_get_channel` (ChatManager.cpp:9285-9638) — the ten-stage
    /// supergroup/broadcast application.
    fn on_get_channel(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        channel: telegram_api::channel,
        source: &str,
    ) {
        let mut channel = channel;
        let channel_id = ChannelId::new(channel.id);
        if !channel_id.is_valid() {
            tdutils::log_error!("Receive invalid {channel_id:?} from {source}");
            return;
        }

        // stage 0: the empty-object stub the server sends for unseen
        // channels
        if channel.flags == 0 && channel.access_hash == 0 && channel.title.is_empty() {
            let c = self.get_channel_force(user_manager, env, channel_id, source, false);
            if c.is_some() {
                tdutils::log_error!(
                    "Receive from {source} empty {channel_id:?}, have a supergroup"
                );
            }
            if c.is_none() && !self.have_min_channel(channel_id) {
                self.min_channels
                    .insert(channel_id.get(), MinChannel::default());
            }
            return;
        }

        // stage 1: locals
        let is_min = channel.min;
        let access_hash = channel.access_hash;
        let has_linked_channel = channel.has_link;
        let sign_messages_initial = channel.signatures;
        let join_to_send = channel.join_to_send;
        let join_request = channel.join_request;
        let is_slow_mode_enabled_initial = channel.slowmode_enabled;
        let is_megagroup = channel.megagroup;
        let is_verified = channel.verified;
        let is_scam = channel.scam;
        let is_fake = channel.fake;
        let is_gigagroup = channel.gigagroup;
        let is_forum = channel.forum;
        let is_forum_tabs = channel.forum_tabs;
        let is_monoforum = channel.monoforum;
        let have_participant_count = channel.flags & (1 << 17) != 0;
        let mut participant_count = channel.participants_count;
        let stories_unavailable = channel.stories_unavailable;
        let mut show_message_sender = channel.signature_profiles;
        let boost_level = channel.level;
        let mut paid_message_star_count = channel.send_paid_messages_stars;
        let autotranslation = channel.autotranslation;
        let mut broadcast_messages_allowed = channel.broadcast_messages_allowed;
        let mut monoforum_channel_id = ChannelId::new(channel.linked_monoforum_id);
        let mut linked_community_id = CommunityId::new(channel.linked_community_id);
        let stories_available = channel
            .stories_max_id
            .as_ref()
            .is_some_and(|stories| stories.max_id > 0);

        // stage 2: normalization
        if (monoforum_channel_id != ChannelId::default() && !monoforum_channel_id.is_valid())
            || monoforum_channel_id == channel_id
        {
            tdutils::log_error!(
                "Receive channel direct messages {monoforum_channel_id:?} for {channel_id:?}"
            );
            monoforum_channel_id = ChannelId::default();
        }
        if !linked_community_id.is_valid() {
            linked_community_id = CommunityId::default();
        }
        if have_participant_count
            && let Some(channel_full) = self.get_channel_full_const(channel_id)
            && channel_full.administrator_count > participant_count
        {
            // must run before the participant-count diff below
            participant_count = channel_full.administrator_count;
        }
        if channel.broadcast == is_megagroup {
            tdutils::log_error!(
                "Receive wrong channel flag is_broadcast == is_megagroup == {is_megagroup} from {source}"
            );
        }
        let mut is_forum_tabs = is_forum_tabs;
        if is_forum_tabs && !is_forum {
            tdutils::log_error!("Receive forum tabs in non-forum {channel_id:?} from {source}");
            is_forum_tabs = false;
        }
        let mut sign_messages = sign_messages_initial;
        let mut is_slow_mode_enabled = is_slow_mode_enabled_initial;
        let mut is_gigagroup = is_gigagroup;
        let mut is_forum = is_forum;
        let mut is_monoforum = is_monoforum;
        if is_megagroup {
            if sign_messages {
                tdutils::log_error!("Need to sign messages in the {channel_id:?} from {source}");
            }
            if broadcast_messages_allowed {
                tdutils::log_error!(
                    "Receive supergroup with channel direct messages group as {channel_id:?} from {source}"
                );
            }
            sign_messages = true;
            show_message_sender = true;
            broadcast_messages_allowed = false;
        } else {
            if is_slow_mode_enabled && channel_id.get() >= 8_000_000_000 {
                tdutils::log_error!("Slow mode enabled in the {channel_id:?} from {source}");
            }
            if is_gigagroup {
                tdutils::log_error!("Receive broadcast gigagroup as {channel_id:?}");
            }
            if is_forum {
                tdutils::log_error!("Receive broadcast forum as {channel_id:?}");
            }
            if is_monoforum {
                tdutils::log_error!("Receive broadcast monoforum group as {channel_id:?}");
            }
            is_slow_mode_enabled = false;
            is_gigagroup = false;
            is_forum = false;
            is_forum_tabs = false;
            is_monoforum = false;
            paid_message_star_count = 0;
        }
        if is_gigagroup {
            // td_->suggested_action_manager_->remove_dialog_suggested_action(
            // SuggestedAction::ConvertToGigagroup) — the M.13 stub.
        }

        // stage 3: the min-channel branch
        if is_min {
            let Some(c) = self.get_channel_force(user_manager, env, channel_id, source, false)
            else {
                // unknown channel: build the degraded MinChannel placeholder
                let mut photo = DialogPhoto::get_dialog_photo(std::mem::take(&mut channel.photo));
                if env.auth_is_bot {
                    photo.minithumbnail = String::new();
                }
                let peer_color = channel
                    .color
                    .as_ref()
                    .map(PeerColor::from_telegram)
                    .unwrap_or_default();
                let min_channel = MinChannel {
                    title_: std::mem::take(&mut channel.title),
                    photo_: photo,
                    accent_color_id_: peer_color.accent_color_id(),
                    is_megagroup_: is_megagroup,
                };
                self.min_channels.insert(channel_id.get(), min_channel);
                return;
            };
            let old_join_to_send = Self::get_channel_join_to_send(c);
            let old_join_request = Self::get_channel_join_request(c);
            Self::on_update_channel_title(c, channel_id, std::mem::take(&mut channel.title));
            let usernames = Usernames::from_telegram(
                std::mem::take(&mut channel.username),
                std::mem::take(&mut channel.usernames),
            );
            self.on_update_channel_usernames(user_manager, env, channel_id, usernames);

            // min updates must not resurrect a photo over a banned status
            let banned = self
                .get_channel(channel_id)
                .is_some_and(|c| c.status.is_banned());
            if !banned {
                let photo = std::mem::take(&mut channel.photo).unwrap_or_default();
                self.on_update_channel_photo(user_manager, env, channel_id, photo);
            }
            {
                let c = self.get_channel_mut(channel_id).expect("known min channel");
                Self::on_update_channel_has_location(c, channel.has_geo);
                Self::on_update_channel_noforwards(c, channel.noforwards);
                Self::on_update_channel_is_forum(c, is_forum, is_forum_tabs);
                let emoji_status = EmojiStatus::get_emoji_status(
                    std::mem::take(&mut channel.emoji_status).unwrap_or_default(),
                );
                Self::on_update_channel_emoji_status(c, channel_id, emoji_status);

                let changed = c.has_linked_channel != has_linked_channel
                    || c.is_slow_mode_enabled != is_slow_mode_enabled
                    || c.is_megagroup != is_megagroup
                    || c.is_scam != is_scam
                    || c.is_fake != is_fake
                    || c.is_gigagroup != is_gigagroup
                    || c.is_monoforum != is_monoforum
                    || c.boost_level != boost_level
                    || c.paid_message_star_count != paid_message_star_count
                    || c.autotranslation != autotranslation
                    || c.broadcast_messages_allowed != broadcast_messages_allowed;
                if changed {
                    c.has_linked_channel = has_linked_channel;
                    c.is_slow_mode_enabled = is_slow_mode_enabled;
                    c.is_megagroup = is_megagroup;
                    c.is_scam = is_scam;
                    c.is_fake = is_fake;
                    c.is_gigagroup = is_gigagroup;
                    c.is_monoforum = is_monoforum;
                    c.boost_level = boost_level;
                    c.paid_message_star_count = paid_message_star_count;
                    c.autotranslation = autotranslation;
                    c.broadcast_messages_allowed = broadcast_messages_allowed;
                    c.is_changed = true;
                }
                if !env.auth_is_bot {
                    let restriction_reasons =
                        get_restriction_reasons(std::mem::take(&mut channel.restriction_reason));
                    if restriction_reasons != c.restriction_reasons {
                        c.restriction_reasons = restriction_reasons;
                        c.is_changed = true;
                    }
                    Self::on_update_channel_bot_verification_icon(
                        c,
                        CustomEmojiId::new(channel.bot_verification_icon),
                    );
                }
                let join_quad_changed = c.join_to_send != join_to_send
                    || c.join_request != join_request
                    || c.monoforum_channel_id != monoforum_channel_id
                    || c.linked_community_id != linked_community_id;
                if join_quad_changed {
                    c.join_to_send = join_to_send;
                    c.join_request = join_request;
                    c.monoforum_channel_id = monoforum_channel_id;
                    c.linked_community_id = linked_community_id;
                    c.need_save_to_database = true;
                }
                // sign_messages isn't known for min-channels
                if c.is_verified != is_verified {
                    c.is_verified = is_verified;
                    c.is_changed = true;
                }
                if old_join_to_send != Self::get_channel_join_to_send(c)
                    || old_join_request != Self::get_channel_join_request(c)
                {
                    c.is_changed = true;
                }
                // must be after setting of c->is_megagroup
                Self::on_update_channel_default_permissions(
                    c,
                    channel_id,
                    RestrictedRights::from_chat_banned_rights(
                        channel.default_banned_rights.as_ref(),
                        ChannelType::Megagroup,
                    ),
                );
                let drop_slow_mode = !c.is_slow_mode_enabled;
                if changed {
                    self.invalidate_channel_full(
                        user_manager,
                        env,
                        channel_id,
                        drop_slow_mode,
                        "on_get_min_channel",
                    );
                }
            }
            self.update_channel(user_manager, env, channel_id, false, false);
            return;
        }

        // stage 4: the access-hash gate
        if access_hash == 0 {
            tdutils::log_error!("Receive non-min {channel_id:?} without access_hash from {source}");
            return;
        }

        // stage 5: the status derivation
        let channel_type = if is_megagroup {
            ChannelType::Megagroup
        } else {
            ChannelType::Broadcast
        };
        let status = if channel.creator {
            DialogParticipantStatus::creator(!channel.left, false, String::new())
        } else if channel.flags & (1 << 13) != 0 {
            DialogParticipantStatus::administrator(
                AdministratorRights::from_chat_admin_rights(
                    channel.admin_rights.as_ref(),
                    channel_type,
                ),
                String::new(),
                false,
            )
        } else if channel.flags & (1 << 15) != 0 {
            DialogParticipantStatus::from_channel_banned_rights(
                !channel.left,
                channel.banned_rights.unwrap(),
                channel_type,
                String::new(),
            )
        } else if channel.left {
            DialogParticipantStatus::left()
        } else {
            DialogParticipantStatus::member(channel.subscription_until_date, String::new())
        };

        // stage 6: the creator pre-load
        if status.is_creator() {
            self.get_channel_force(user_manager, env, channel_id, source, false);
        }

        // stage 7: the monoforum admin side-effects
        let mut is_admined_monoforum = false;
        if monoforum_channel_id.is_valid() {
            if is_monoforum {
                if let Some(monoforum_c) =
                    self.get_channel_force(user_manager, env, monoforum_channel_id, source, false)
                {
                    is_admined_monoforum = monoforum_c.status.can_manage_direct_messages();
                } else if status.is_member() && env.auth_is_bot {
                    is_admined_monoforum = true;
                }
            } else if let Some(monoforum_c) = self.get_channel_mut(monoforum_channel_id)
                && status.can_manage_direct_messages() != monoforum_c.is_admined_monoforum
            {
                monoforum_c.is_admined_monoforum = status.can_manage_direct_messages();
                monoforum_c.is_admined_monoforum_changed = true;
                monoforum_c.is_changed = true;
                self.update_channel(user_manager, env, monoforum_channel_id, false, false);
            }
        }

        // stage 8: the record application
        let record_was_empty = !self.channels.contains_key(&channel_id.get());
        self.add_channel(channel_id);
        if record_was_empty {
            // the first real creation erases the MinChannel placeholder
            self.min_channels.remove(&channel_id.get());
        }
        let old_join_to_send;
        let old_join_request;
        {
            let c = self.add_channel(channel_id);
            old_join_to_send = Self::get_channel_join_to_send(c);
            old_join_request = Self::get_channel_join_request(c);
        }
        let mut need_invalidate_channel_full = false;
        let mut need_update_participant_count = false;
        {
            let c = self.add_channel(channel_id);
            if c.access_hash != access_hash {
                c.access_hash = access_hash;
                c.need_save_to_database = true;
            }
            if c.date != channel.date {
                c.date = channel.date;
                c.is_changed = true;
            }
            if have_participant_count && participant_count != c.participant_count {
                need_update_participant_count = true;
                c.participant_count = participant_count;
                c.is_changed = true;
            }
            let changed = c.has_linked_channel != has_linked_channel
                || c.is_slow_mode_enabled != is_slow_mode_enabled
                || c.is_megagroup != is_megagroup
                || c.is_scam != is_scam
                || c.is_fake != is_fake
                || c.is_gigagroup != is_gigagroup
                || c.is_monoforum != is_monoforum
                || c.boost_level != boost_level
                || c.paid_message_star_count != paid_message_star_count
                || c.broadcast_messages_allowed != broadcast_messages_allowed;
            if changed {
                c.has_linked_channel = has_linked_channel;
                c.is_slow_mode_enabled = is_slow_mode_enabled;
                c.is_megagroup = is_megagroup;
                c.is_scam = is_scam;
                c.is_fake = is_fake;
                c.is_gigagroup = is_gigagroup;
                c.is_monoforum = is_monoforum;
                c.boost_level = boost_level;
                c.paid_message_star_count = paid_message_star_count;
                c.broadcast_messages_allowed = broadcast_messages_allowed;
                c.is_changed = true;
                need_invalidate_channel_full = true;
            }
            if !env.auth_is_bot {
                let restriction_reasons =
                    get_restriction_reasons(std::mem::take(&mut channel.restriction_reason));
                if restriction_reasons != c.restriction_reasons {
                    c.restriction_reasons = restriction_reasons;
                    c.is_changed = true;
                }
                Self::on_update_channel_bot_verification_icon(
                    c,
                    CustomEmojiId::new(channel.bot_verification_icon),
                );
            }
            let join_quad_changed = c.join_to_send != join_to_send
                || c.join_request != join_request
                || c.monoforum_channel_id != monoforum_channel_id
                || c.linked_community_id != linked_community_id;
            if join_quad_changed {
                c.join_to_send = join_to_send;
                c.join_request = join_request;
                c.monoforum_channel_id = monoforum_channel_id;
                c.linked_community_id = linked_community_id;
                c.need_save_to_database = true;
            }
            let verify_quad_changed = c.is_verified != is_verified
                || c.sign_messages != sign_messages
                || c.show_message_sender != show_message_sender
                || c.autotranslation != autotranslation;
            if verify_quad_changed {
                c.is_verified = is_verified;
                c.sign_messages = sign_messages;
                c.show_message_sender = show_message_sender;
                c.autotranslation = autotranslation;
                c.is_changed = true;
            }
            if old_join_to_send != Self::get_channel_join_to_send(c)
                || old_join_request != Self::get_channel_join_request(c)
            {
                c.is_changed = true;
            }
            if c.is_admined_monoforum != is_admined_monoforum {
                c.is_admined_monoforum = is_admined_monoforum;
                c.is_admined_monoforum_changed = true;
                c.is_changed = true;
            }
        }

        // stage 9: the on_update application order
        Self::on_update_channel_title(
            self.add_channel(channel_id),
            channel_id,
            std::mem::take(&mut channel.title),
        );
        {
            let photo = std::mem::take(&mut channel.photo).unwrap_or_default();
            self.on_update_channel_photo(user_manager, env, channel_id, photo);
        }
        let peer_color = channel
            .color
            .as_ref()
            .map(PeerColor::from_telegram)
            .unwrap_or_default();
        Self::on_update_channel_accent_color_id(
            self.add_channel(channel_id),
            channel_id,
            peer_color.accent_color_id(),
        );
        Self::on_update_channel_background_custom_emoji_id(
            self.add_channel(channel_id),
            peer_color.background_custom_emoji_id(),
        );
        let profile_peer_color = channel
            .profile_color
            .as_ref()
            .map(PeerColor::from_telegram)
            .unwrap_or_default();
        Self::on_update_channel_profile_accent_color_id(
            self.add_channel(channel_id),
            profile_peer_color.accent_color_id(),
        );
        Self::on_update_channel_profile_background_custom_emoji_id(
            self.add_channel(channel_id),
            profile_peer_color.background_custom_emoji_id(),
        );
        self.on_update_channel_status(user_manager, env, channel_id, status);
        {
            let usernames = Usernames::from_telegram(
                std::mem::take(&mut channel.username),
                std::mem::take(&mut channel.usernames),
            );
            // uses the (new) status; must run after on_update_channel_status
            self.on_update_channel_usernames(user_manager, env, channel_id, usernames);
        }
        Self::on_update_channel_has_location(self.add_channel(channel_id), channel.has_geo);
        Self::on_update_channel_noforwards(self.add_channel(channel_id), channel.noforwards);
        Self::on_update_channel_is_forum(self.add_channel(channel_id), is_forum, is_forum_tabs);
        {
            let emoji_status = EmojiStatus::get_emoji_status(
                std::mem::take(&mut channel.emoji_status).unwrap_or_default(),
            );
            Self::on_update_channel_emoji_status(
                self.add_channel(channel_id),
                channel_id,
                emoji_status,
            );
        }
        if !env.auth_is_bot && !channel.stories_hidden_min {
            Self::on_update_channel_stories_hidden(
                env,
                self.add_channel(channel_id),
                channel.stories_hidden,
            );
        }
        // must be after setting of c->is_megagroup
        Self::on_update_channel_default_permissions(
            self.add_channel(channel_id),
            channel_id,
            RestrictedRights::from_chat_banned_rights(
                channel.default_banned_rights.as_ref(),
                ChannelType::Megagroup,
            ),
        );
        // update at the end, because it calls need_poll_channel_active_stories
        if !env.auth_is_bot && (stories_available || stories_unavailable) {
            let stories_max_id = std::mem::take(&mut channel.stories_max_id).unwrap_or_default();
            self.on_update_channel_story_ids_impl(
                env,
                channel_id,
                stories_max_id,
                StoryId::default(),
            );
        }

        // stage 10: finalize
        {
            let c = self.add_channel(channel_id);
            if c.cache_version != Channel::CACHE_VERSION {
                c.cache_version = Channel::CACHE_VERSION;
                c.need_save_to_database = true;
            }
            c.is_received_from_server = true;
        }
        self.update_channel(user_manager, env, channel_id, false, false);
        if need_update_participant_count
            && let Some(channel_full) =
                self.get_channel_full(env, channel_id, true, "on_get_channel")
            && channel_full.participant_count != participant_count
        {
            channel_full.participant_count = participant_count;
            channel_full.is_changed = true;
            self.update_channel_full(user_manager, env, channel_id, "on_get_channel", false);
        }
        if need_invalidate_channel_full {
            let drop = !self
                .get_channel(channel_id)
                .is_some_and(|c| c.is_slow_mode_enabled);
            self.invalidate_channel_full(user_manager, env, channel_id, drop, "on_get_channel");
        }
        // td_->messages_manager_->on_update_dialog_group_call — the M.10 stub.
        if monoforum_channel_id.is_valid()
            && !env.auth_is_bot
            && !self.have_channel(monoforum_channel_id)
        {
            // reloads the channel's own full, not the monoforum's
            self.reload_channel_full(env, channel_id, Promise::default(), "load monoforum");
        }
    }

    /// `on_get_channel_forbidden` (ChatManager.cpp:9640-9759).
    fn on_get_channel_forbidden(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        mut channel: telegram_api::channelForbidden,
        source: &str,
    ) {
        let channel_id = ChannelId::new(channel.id);
        if !channel_id.is_valid() {
            tdutils::log_error!("Receive invalid {channel_id:?} from {source}");
            return;
        }
        if channel.flags == 0 && channel.access_hash == 0 && channel.title.is_empty() {
            let c = self.get_channel_force(user_manager, env, channel_id, source, false);
            tdutils::log_error!(
                "Receive empty forbidden {channel_id:?} from {source}, have a supergroup"
            );
            if c.is_none() && !self.have_min_channel(channel_id) {
                self.min_channels
                    .insert(channel_id.get(), MinChannel::default());
            }
            return;
        }

        let sign_messages;
        let show_message_sender;
        let join_to_send = false;
        let join_request = false;
        let is_slow_mode_enabled = false;
        let is_megagroup = channel.megagroup;
        let is_verified = false;
        let is_scam = false;
        let is_fake = false;
        let autotranslation = false;
        let broadcast_messages_allowed = false;
        let is_monoforum = channel.monoforum;
        let is_admined_monoforum = false;

        if channel.broadcast == is_megagroup {
            tdutils::log_error!(
                "Receive wrong channel flag is_broadcast == is_megagroup == {is_megagroup} from {source}"
            );
        }
        if is_megagroup {
            sign_messages = true;
            show_message_sender = true;
        } else {
            sign_messages = false;
            show_message_sender = false;
        }

        self.add_channel(channel_id);
        let old_join_to_send;
        let old_join_request;
        {
            let c = self.add_channel(channel_id);
            old_join_to_send = Self::get_channel_join_to_send(c);
            old_join_request = Self::get_channel_join_request(c);
        }
        let mut need_invalidate_channel_full = false;
        {
            let c = self.add_channel(channel_id);
            if c.access_hash != channel.access_hash {
                c.access_hash = channel.access_hash;
                c.need_save_to_database = true;
            }
            if c.date != 0 {
                c.date = 0;
                c.is_changed = true;
            }
            // the deliberate C++ commented-out has_linked_channel line
            let changed = c.is_slow_mode_enabled != is_slow_mode_enabled
                || c.is_megagroup != is_megagroup
                || !c.restriction_reasons.is_empty()
                || c.is_scam != is_scam
                || c.is_fake != is_fake
                || c.join_to_send != join_to_send
                || c.join_request != join_request
                || c.broadcast_messages_allowed != broadcast_messages_allowed
                || c.is_monoforum != is_monoforum;
            if changed {
                c.is_slow_mode_enabled = is_slow_mode_enabled;
                c.is_megagroup = is_megagroup;
                c.restriction_reasons.clear();
                c.is_scam = is_scam;
                c.is_fake = is_fake;
                c.join_to_send = join_to_send;
                c.join_request = join_request;
                c.broadcast_messages_allowed = broadcast_messages_allowed;
                c.is_monoforum = is_monoforum;
                c.is_changed = true;
                need_invalidate_channel_full = true;
            }
            if c.is_monoforum && c.monoforum_channel_id == ChannelId::default() {
                let offset: i64 = if env.global.is_test_dc() {
                    1_300_000_000_000
                } else {
                    1_070_000_000_000
                };
                let expected_monoforum_channel_id = ChannelId::new(channel_id.get() - offset);
                if expected_monoforum_channel_id.is_valid() {
                    c.monoforum_channel_id = expected_monoforum_channel_id;
                    c.need_save_to_database = true;
                }
            }
            if c.join_to_send != join_to_send || c.join_request != join_request {
                c.join_to_send = join_to_send;
                c.join_request = join_request;
                c.need_save_to_database = true;
            }
            let verify_quad_changed = c.is_verified != is_verified
                || c.sign_messages != sign_messages
                || c.show_message_sender != show_message_sender
                || c.autotranslation != autotranslation;
            if verify_quad_changed {
                c.is_verified = is_verified;
                c.sign_messages = sign_messages;
                c.show_message_sender = show_message_sender;
                c.autotranslation = autotranslation;
                c.is_changed = true;
            }
            if old_join_to_send != Self::get_channel_join_to_send(c)
                || old_join_request != Self::get_channel_join_request(c)
            {
                c.is_changed = true;
            }
            if c.is_admined_monoforum != is_admined_monoforum {
                c.is_admined_monoforum = false;
                c.is_admined_monoforum_changed = true;
                c.is_changed = true;
            }
            Self::on_update_channel_title(c, channel_id, std::mem::take(&mut channel.title));
        }
        {
            let photo = telegram_api::ChatPhoto::chatPhotoEmpty(telegram_api::chatPhotoEmpty {});
            self.on_update_channel_photo(user_manager, env, channel_id, photo);
        }
        // deliberately not called: on_update_channel_usernames /
        // on_update_channel_has_location — we don't know whether the
        // channel usernames are empty, so don't update them
        {
            let c = self.add_channel(channel_id);
            Self::on_update_channel_noforwards(c, false);
            Self::on_update_channel_emoji_status(c, channel_id, None);
            Self::on_update_channel_bot_verification_icon(c, CustomEmojiId::default());
            // td_->messages_manager_->on_update_dialog_group_call — the M.10 stub.
            // must be after setting of c->is_megagroup
            Self::on_update_channel_default_permissions(
                c,
                channel_id,
                RestrictedRights::from_chat_banned_rights(None, ChannelType::Megagroup),
            );
        }
        self.on_update_channel_status(
            user_manager,
            env,
            channel_id,
            DialogParticipantStatus::banned(
                if channel.flags & (1 << 16) != 0 {
                    channel.until_date
                } else {
                    0
                },
                String::new(),
            ),
        );
        {
            let c = self.add_channel(channel_id);
            if c.participant_count != 0 {
                c.participant_count = 0;
                c.is_changed = true;
            }
            if c.cache_version != Channel::CACHE_VERSION {
                c.cache_version = Channel::CACHE_VERSION;
                c.need_save_to_database = true;
            }
            c.is_received_from_server = true;
        }
        self.update_channel(user_manager, env, channel_id, false, false);

        if self
            .get_channel(channel_id)
            .is_some_and(|c| c.participant_count != 0)
            && let Some(channel_full) =
                self.get_channel_full(env, channel_id, true, "on_get_channel_forbidden")
            && channel_full.participant_count != 0
        {
            channel_full.participant_count = 0;
            channel_full.administrator_count = 0;
            channel_full.is_changed = true;
            self.update_channel_full(
                user_manager,
                env,
                channel_id,
                "on_get_channel_forbidden 2",
                false,
            );
        }
        if need_invalidate_channel_full {
            let drop = !self
                .get_channel(channel_id)
                .is_some_and(|c| c.is_slow_mode_enabled);
            self.invalidate_channel_full(
                user_manager,
                env,
                channel_id,
                drop,
                "on_get_channel_forbidden 3",
            );
        }
    }

    // ---- M.5.27: the speculative machine and the channel error intake
    // (ChatManager.cpp:6393-6663) ----

    /// `speculative_add_count` (ChatManager.cpp:6466-6477).
    fn speculative_add_count(count: &mut i32, delta_count: i32, min_count: i32) -> bool {
        let mut new_count = *count + delta_count;
        if new_count < min_count {
            new_count = min_count;
        }
        if new_count == *count {
            return false;
        }
        *count = new_count;
        true
    }

    /// `on_get_channel_error` (ChatManager.cpp:6393-6464). Returns true
    /// when the error was handled. The member-leave branch cannot call
    /// the not-yet-ported on_get_channel_forbidden, so the emulation
    /// reduces to the invalidation tail (the channel-intake slice owns
    /// the full form).
    #[allow(dead_code)] // production caller: the net query error paths (M.10)
    fn on_get_channel_error(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        channel_id: ChannelId,
        status: &Status,
        source: &str,
    ) -> bool {
        tdutils::log_info!("Receive {status} in {channel_id:?} from {source}");
        if status.message() == "BOT_METHOD_INVALID" {
            return true;
        }
        if status.message() == "CHANNEL_MONOFORUM_UNSUPPORTED" {
            tdutils::log_error!(
                "Receive CHANNEL_MONOFORUM_UNSUPPORTED in {channel_id:?} from {source}"
            );
            return true;
        }
        if env.global.is_expected_error(status) {
            return true;
        }
        if matches!(
            status.message(),
            "CHANNEL_PRIVATE" | "CHANNEL_PUBLIC_GROUP_NA" | "FROZEN_PARTICIPANT_MISSING"
        ) {
            if !channel_id.is_valid() {
                tdutils::log_error!("Receive {status} in invalid {channel_id:?} from {source}");
                return false;
            }
            let initial_channel_id = channel_id;
            let mut channel_id = channel_id;
            let initial_slow_mode = self
                .get_channel(channel_id)
                .map(|c| c.is_slow_mode_enabled)
                .unwrap_or(false);
            if self.get_channel(channel_id).is_some_and(|c| c.is_monoforum) {
                channel_id = self
                    .get_channel(channel_id)
                    .map(|c| c.monoforum_channel_id)
                    .unwrap_or_default();
            }
            if self.get_channel(channel_id).is_none() {
                if source == "GetChannelDifferenceQuery" || source == "GetChannelsQuery" {
                    // the restart/diff path re-fetches the channel by id
                    return true;
                }
                tdutils::log_error!(
                    "Receive {status} in not found {initial_channel_id:?}/{channel_id:?} from {source}"
                );
                return false;
            }
            // the client-side supergroup snapshot for the tail LOG_IF
            let _debug_channel_object = self.get_supergroup_object(channel_id);

            let is_member = self
                .get_channel(channel_id)
                .is_some_and(|c| c.status.is_member());
            if is_member {
                // the C++ emulates the leave via on_get_channel_forbidden
                // with a synthesized channelForbidden — that intake is the
                // channel slice; the invalidation tail still runs.
            } else if !self
                .get_channel(channel_id)
                .is_some_and(|c| c.status.is_banned())
            {
                // drop usernames when they are known
                let has_usernames = self
                    .get_channel(channel_id)
                    .is_some_and(|c| c.usernames != Usernames::default());
                if has_usernames {
                    tdutils::log_info!("Drop usernames of {channel_id:?}");
                    let usernames = Usernames::from_telegram(String::new(), Vec::new());
                    self.on_update_channel_usernames(user_manager, env, channel_id, usernames);
                }
                if let Some(c) = self.get_channel_mut(channel_id) {
                    Self::on_update_channel_has_location(c, false);
                }
                // the DialogInviteLinkManager remove_dialog_access_by_invite_link
                // — the wave-2 stub.
                self.update_channel(user_manager, env, channel_id, false, false);
            }
            let drop_slow_mode = !self
                .get_channel(channel_id)
                .is_some_and(|c| c.is_slow_mode_enabled);
            self.invalidate_channel_full(user_manager, env, channel_id, drop_slow_mode, source);
            if channel_id != initial_channel_id {
                self.invalidate_channel_full(
                    user_manager,
                    env,
                    initial_channel_id,
                    initial_slow_mode,
                    source,
                );
            }
            if self.have_input_peer_channel_id(env, initial_channel_id, AccessRights::Read) {
                tdutils::log_error!(
                    "Have read access to {initial_channel_id:?} after receiving CHANNEL_PRIVATE"
                );
            }
            return true;
        }
        false
    }

    /// `speculative_add_channel_participants`
    /// (ChatManager.cpp:6479-6512). The DialogParticipantManager cache and
    /// the MessagesManager bots fan-out are the wave-2 stubs.
    #[allow(dead_code)] // production caller: MessagesManager add paths (M.10)
    #[allow(clippy::too_many_arguments)]
    fn speculative_add_channel_participants(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        channel_id: ChannelId,
        added_user_ids: &[UserId],
        _inviter_user_id: UserId,
        _date: i32,
        by_me: bool,
    ) {
        if self
            .get_channel_full_force(
                user_manager,
                env,
                channel_id,
                true,
                "speculative_add_channel_participants",
                false,
            )
            .is_none()
        {
            return;
        }
        let mut delta_participant_count = 0;
        let mut bots_changed = false;
        {
            let Some(f) = self.get_channel_full_mut(channel_id) else {
                return;
            };
            for user_id in added_user_ids.iter().copied() {
                if !user_id.is_valid() {
                    continue;
                }
                delta_participant_count += 1;
                if user_manager.is_user_bot_of(user_id) && !f.bot_user_ids.contains(&user_id) {
                    f.bot_user_ids.push(user_id);
                    f.need_save_to_database = true;
                    bots_changed = true;
                }
            }
        }
        if bots_changed {
            self.reload_channel_full(
                env,
                channel_id,
                Promise::default(),
                "speculative_add_channel_participants",
            );
            // messages_manager on_dialog_bots_updated — the M.10 stub.
        }
        {
            let Some(f) = self.get_channel_full_mut(channel_id) else {
                return;
            };
            if f.is_changed {
                f.speculative_version += 1;
            }
        }
        self.update_channel_full(
            user_manager,
            env,
            channel_id,
            "speculative_add_channel_participants",
            false,
        );
        if delta_participant_count == 0 {
            return;
        }
        self.speculative_add_channel_participant_count(
            user_manager,
            env,
            channel_id,
            delta_participant_count,
            by_me,
        );
    }

    /// `speculative_delete_channel_participant`
    /// (ChatManager.cpp:6514-6538). No speculative_version bump — C++.
    #[allow(dead_code)] // production caller: MessagesManager delete paths (M.10)
    fn speculative_delete_channel_participant(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        channel_id: ChannelId,
        deleted_user_id: UserId,
        by_me: bool,
    ) {
        if !deleted_user_id.is_valid() {
            return;
        }
        // td_->dialog_participant_manager_->delete_cached_channel_participant
        // — the wave-2 stub.
        if user_manager.is_user_bot_of(deleted_user_id)
            && self
                .get_channel_full_force(
                    user_manager,
                    env,
                    channel_id,
                    true,
                    "speculative_delete_channel_participant",
                    false,
                )
                .is_some()
        {
            if let Some(f) = self.get_channel_full_mut(channel_id) {
                if let Some(position) = f.bot_user_ids.iter().position(|id| *id == deleted_user_id)
                {
                    f.bot_user_ids.remove(position);
                    f.need_save_to_database = true;
                    // messages_manager on_dialog_bots_updated — the M.10 stub.
                }
                if f.guard_bot_user_id == deleted_user_id {
                    f.guard_bot_user_id = UserId::default();
                }
            }
            self.update_channel_full(
                user_manager,
                env,
                channel_id,
                "speculative_delete_channel_participant",
                false,
            );
        }
        self.speculative_add_channel_participant_count(user_manager, env, channel_id, -1, by_me);
    }

    /// `speculative_add_channel_participant_count`
    /// (ChatManager.cpp:6540-6570).
    fn speculative_add_channel_participant_count(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        channel_id: ChannelId,
        delta_participant_count: i32,
        by_me: bool,
    ) {
        if by_me {
            // own changes may already be counted — silent full reset
            self.invalidate_channel_full(
                user_manager,
                env,
                channel_id,
                false,
                "speculative_add_channel_participant_count",
            );
            return;
        }
        if self
            .get_channel_full_force(
                user_manager,
                env,
                channel_id,
                true,
                "speculative_add_channel_participant_count",
                false,
            )
            .is_none()
        {
            return;
        }
        let min_count = self
            .get_channel_full_const(channel_id)
            .map(|f| f.administrator_count)
            .unwrap_or(0);
        {
            let Some(c) = self.get_channel_mut(channel_id) else {
                return;
            };
            if c.participant_count != 0
                && Self::speculative_add_count(
                    &mut c.participant_count,
                    delta_participant_count,
                    min_count,
                )
            {
                c.is_changed = true;
            }
        }
        self.update_channel(user_manager, env, channel_id, false, false);
        let changed = {
            let Some(f) = self.get_channel_full_mut(channel_id) else {
                return;
            };
            let changed = Self::speculative_add_count(
                &mut f.participant_count,
                delta_participant_count,
                min_count,
            );
            f.is_changed |= changed;
            if f.is_changed {
                f.speculative_version += 1;
            }
            changed
        };
        if changed {
            self.update_channel_full(
                user_manager,
                env,
                channel_id,
                "speculative_add_channel_participant_count",
                false,
            );
        }
    }

    /// `on_update_channel_bot_commands` (ChatManager.cpp:9112-9119).
    pub fn on_update_channel_bot_commands(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        channel_id: ChannelId,
        bot_commands: BotCommands,
    ) {
        if self
            .get_channel_full_force(
                user_manager,
                env,
                channel_id,
                true,
                "on_update_channel_bot_commands",
                false,
            )
            .is_none()
        {
            return;
        }
        let mut changed = false;
        if let Some(f) = self.get_channel_full_mut(channel_id)
            && BotCommands::update_all_bot_commands(&mut f.bot_commands, bot_commands)
        {
            f.is_changed = true;
            changed = true;
        }
        if changed {
            self.update_channel_full(
                user_manager,
                env,
                channel_id,
                "on_update_channel_bot_commands",
                false,
            );
        }
    }

    /// `on_update_channel_guard_bot_user_id` (ChatManager.cpp:9121-9127).
    pub fn on_update_channel_guard_bot_user_id(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        channel_id: ChannelId,
        guard_bot_user_id: UserId,
    ) {
        if self
            .get_channel_full_force(
                user_manager,
                env,
                channel_id,
                true,
                "on_update_channel_guard_bot_user_id",
                false,
            )
            .is_some()
        {
            self.on_update_channel_full_guard_bot_user_id(
                user_manager,
                channel_id,
                guard_bot_user_id,
            );
            self.update_channel_full(
                user_manager,
                env,
                channel_id,
                "on_update_channel_guard_bot_user_id",
                false,
            );
        }
    }

    /// `on_update_channel_permanent_invite_link`
    /// (ChatManager.cpp:9144-9152).
    pub fn on_update_channel_permanent_invite_link(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        channel_id: ChannelId,
        invite_link: &DialogInviteLink,
    ) {
        if self
            .get_channel_full_force(
                user_manager,
                env,
                channel_id,
                true,
                "on_update_channel_permanent_invite_link",
                false,
            )
            .is_none()
        {
            return;
        }
        let mut changed = false;
        if let Some(f) = self.get_channel_full_mut(channel_id)
            && Self::update_permanent_invite_link(&mut f.invite_link, invite_link.clone())
        {
            f.is_changed = true;
            changed = true;
        }
        if changed {
            self.update_channel_full(
                user_manager,
                env,
                channel_id,
                "on_update_channel_permanent_invite_link",
                false,
            );
        }
    }

    // ---- M.5.34/M.5.35/M.5.37: the read surface ----

    /// `get_chat` (ChatManager.cpp:8397-8419) — the left-tries ladder.
    /// Renamed `try_get_chat` here: the landed `get_chat` record accessor
    /// owns the C++ record-form overload.
    /// The db branch is the synchronous-kv load; the queue branch is the
    /// landed net-blocked GetChatsQuery error.
    pub fn try_get_chat(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        chat_id: ChatId,
        left_tries: i32,
        mut promise: Promise<()>,
    ) -> bool {
        if !chat_id.is_valid() {
            promise.set_error(Status::error(400, "Invalid basic group identifier"));
            return false;
        }
        if !self.have_chat(chat_id) {
            if left_tries > 2 && env.global.use_chat_info_database() {
                self.load_chat_from_database(user_manager, env, chat_id, false);
                return false;
            }
            if left_tries > 1 {
                // get_chat_queries_.add_query — the deduped net query is
                // [blocked: net] (M.5.44)
                promise.set_error(Status::error(500, "Network is not implemented yet"));
                return false;
            }
            promise.set_error(Status::error(400, "Group not found"));
            return false;
        }
        promise.set_value(());
        true
    }

    /// `get_channel` (ChatManager.cpp:8928-8936) — the left-tries ladder
    /// with the bot-gated queue branch and the monoforum tail. Renamed
    /// `try_get_channel` (record accessor owns the C++ overload).
    pub fn try_get_channel(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        channel_id: ChannelId,
        left_tries: i32,
        mut promise: Promise<()>,
    ) -> bool {
        if !channel_id.is_valid() {
            promise.set_error(Status::error(400, "Invalid supergroup identifier"));
            return false;
        }
        if !self.have_channel(channel_id) {
            if left_tries > 2 && env.auth_is_bot {
                self.load_channel_from_database(user_manager, env, channel_id, false, false);
                return false;
            }
            if left_tries > 1 {
                promise.set_error(Status::error(500, "Network is not implemented yet"));
                return false;
            }
            promise.set_error(Status::error(400, "Supergroup not found"));
            return false;
        }
        let monoforum_channel_id = self
            .get_channel(channel_id)
            .map(|c| c.monoforum_channel_id)
            .unwrap_or_default();
        if monoforum_channel_id.is_valid() && !self.have_channel(monoforum_channel_id) {
            if left_tries > 3 && env.global.use_chat_info_database() {
                self.load_channel_from_database(
                    user_manager,
                    env,
                    monoforum_channel_id,
                    false,
                    false,
                );
            } else if left_tries > 2 && env.auth_is_bot {
                promise.set_error(Status::error(500, "Network is not implemented yet"));
                return false;
            } else {
                promise.set_value(());
                return true;
            }
        }
        promise.set_value(());
        true
    }

    /// `get_channel_type` (ChatManager.cpp:8605-8617) — the id-level
    /// fallback ladder through the min-channel cache.
    #[allow(dead_code)] // production callers: MessagesManager/updates (M.10)
    pub(crate) fn get_channel_type_by_id(&self, channel_id: ChannelId) -> ChannelType {
        if let Some(c) = self.get_channel(channel_id) {
            return Self::get_channel_type(c);
        }
        if let Some(min) = self.get_min_channel(channel_id) {
            return if min.is_megagroup_ {
                ChannelType::Megagroup
            } else {
                ChannelType::Broadcast
            };
        }
        ChannelType::Unknown
    }

    /// `get_unsupported_channel_id` (ChatManager.cpp:3161-3163).
    pub fn get_unsupported_channel_id(&self, env: &UserEnv) -> ChannelId {
        ChannelId::new(if env.global.is_test_dc() {
            10_304_875
        } else {
            1_535_424_647
        })
    }

    /// `get_chat_status` (ChatManager.cpp:8634-8640).
    pub fn get_chat_status(&self, chat_id: ChatId) -> DialogParticipantStatus {
        match self.get_chat(chat_id) {
            Some(c) => c.status.clone(),
            None => DialogParticipantStatus::banned(0, String::new()),
        }
    }

    /// `get_chat_participant_count` (ChatManager.cpp:8622-8626).
    pub fn get_chat_participant_count(&self, chat_id: ChatId) -> i32 {
        self.get_chat(chat_id)
            .map(|c| c.participant_count)
            .unwrap_or(0)
    }

    /// `get_chat_is_active` (ChatManager.cpp:8628-8632).
    pub fn get_chat_is_active(&self, chat_id: ChatId) -> bool {
        self.get_chat(chat_id).is_some_and(|c| c.is_active)
    }

    /// `get_chat_date` (ChatManager.cpp:8612-8616).
    pub fn get_chat_date(&self, chat_id: ChatId) -> i32 {
        self.get_chat(chat_id).map(|c| c.date).unwrap_or(0)
    }

    /// `get_channel_status` (ChatManager.cpp:8642-8648).
    pub fn get_channel_status(&self, channel_id: ChannelId) -> DialogParticipantStatus {
        match self.get_channel(channel_id) {
            Some(c) => c.status.clone(),
            None => DialogParticipantStatus::banned(0, String::new()),
        }
    }

    /// `get_channel_date` (ChatManager.cpp:8619-8623) and
    /// `get_channel_participant_count` (8625-8629) and the plain flag
    /// getters (8631-8655 region) collapsed onto their record fields.
    pub fn get_channel_field_snapshot(&self, channel_id: ChannelId) -> (i32, i32, bool, bool) {
        match self.get_channel(channel_id) {
            Some(c) => (c.date, c.participant_count, c.is_verified, c.sign_messages),
            None => (0, 0, false, false),
        }
    }

    /// `get_current_state` (ChatManager.cpp:9966-9992) — the state-dump
    /// terminal piece. Order contract: unknown placeholders first; channels
    /// before chats ("chat objects can contain channel_id, so they must be
    /// sent after channels" — cpp:9977); fulls last, channels-full before
    /// chats-full. Intra-container order is hash order in the C++ and
    /// sorted in Rust — differential tests compare per-container sets.
    pub fn get_current_state(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        updates: &mut Vec<td_api::Update>,
    ) {
        for chat_id in self.unknown_chats.iter().copied() {
            let chat_id = ChatId::new(chat_id);
            if !self.have_chat(chat_id) {
                updates.push(Self::get_update_unknown_basic_group_object(chat_id));
            }
        }
        for channel_id in self.unknown_channels.iter().copied() {
            let channel_id = ChannelId::new(channel_id);
            if !self.have_channel(channel_id) {
                updates.push(self.get_update_unknown_supergroup_object(channel_id));
            }
        }
        for raw_channel_id in self.channels.keys().copied().collect::<Vec<_>>() {
            let channel_id = ChannelId::new(raw_channel_id);
            let c = self.get_channel(channel_id).cloned();
            updates.push(self.get_update_supergroup_object(channel_id, c.as_ref()));
        }
        for raw_chat_id in self.chats.keys().copied().collect::<Vec<_>>() {
            let chat_id = ChatId::new(raw_chat_id);
            let c = self.get_chat(chat_id).cloned();
            updates.push(self.get_update_basic_group_object(chat_id, c.as_ref()));
        }
        let channel_full_ids: Vec<i64> = self.channels_full.keys().copied().collect::<Vec<_>>();
        for channel_id in channel_full_ids {
            let channel_id = ChannelId::new(channel_id);
            let c = self.get_channel(channel_id).cloned();
            debug_assert!(
                self.get_channel_full_const(channel_id)
                    .is_some_and(|f| f.is_update_channel_full_sent)
            );
            let update = self.get_update_supergroup_full_info_object(
                user_manager,
                env,
                channel_id,
                "get_current_state",
            );
            updates.push(update);
            let _ = c;
        }
        let chat_full_ids: Vec<i64> = self.chats_full.keys().copied().collect::<Vec<_>>();
        for chat_id in chat_full_ids {
            let basic_group_id =
                self.get_basic_group_id_object(ChatId::new(chat_id), "get_current_state");
            let full_object =
                self.get_basic_group_full_info_object(user_manager, env, ChatId::new(chat_id));
            debug_assert!(
                self.get_chat_full(ChatId::new(chat_id))
                    .is_some_and(|f| f.is_update_chat_full_sent)
            );
            updates.push(td_api::Update::updateBasicGroupFullInfo(
                td_api::updateBasicGroupFullInfo {
                    basic_group_id,
                    basic_group_full_info: Some(full_object),
                },
            ));
        }
    }

    // ---- M.5.38-M.5.43: permission getters and the setter/query surface
    // (ChatManager.cpp:3165-4045) ----

    /// `get_chat_permissions(ChatId)` (ChatManager.cpp:8574-8580).
    pub fn get_chat_permissions_id(&self, chat_id: ChatId) -> DialogParticipantStatus {
        match self.get_chat(chat_id) {
            Some(c) => Self::get_chat_permissions_record(c),
            None => DialogParticipantStatus::banned(0, String::new()),
        }
    }

    /// `is_appointed_chat_administrator` (ChatManager.cpp:8589-8595) — the
    /// bot-side rights probe of `setChatAvailableReactions`.
    pub fn is_appointed_chat_administrator(&self, chat_id: ChatId) -> bool {
        self.get_chat(chat_id)
            .is_some_and(|c| c.status.is_administrator())
    }

    /// `get_chat_permissions(const Chat *)` (ChatManager.cpp:8582-8588).
    pub fn get_chat_permissions_record(c: &Chat) -> DialogParticipantStatus {
        if !c.is_active {
            return DialogParticipantStatus::banned(0, String::new());
        }
        c.status
            .apply_restrictions(c.default_permissions, false, false)
    }

    /// `get_channel_permissions(ChannelId)` (ChatManager.cpp:8693-8699).
    pub fn get_channel_permissions_id(
        &self,
        global: &Global,
        channel_id: ChannelId,
    ) -> DialogParticipantStatus {
        match self.get_channel(channel_id) {
            Some(c) => self.get_channel_permissions_channel(global, channel_id, c),
            None => DialogParticipantStatus::banned(0, String::new()),
        }
    }

    /// `get_channel_permissions(ChannelId, const Channel *)`
    /// (ChatManager.cpp:8701-8714) — megagroup boosters bypass the
    /// default restrictions when the full says they boosted enough.
    pub(crate) fn get_channel_permissions_channel(
        &self,
        global: &Global,
        channel_id: ChannelId,
        c: &Channel,
    ) -> DialogParticipantStatus {
        let mut status = c.status.clone();
        status.update_restrictions(global);
        let mut is_booster = false;
        if c.is_megagroup {
            is_booster = self.get_channel_full_const(channel_id).is_none_or(|f| {
                f.unrestrict_boost_count == 0 || f.boost_count >= f.unrestrict_boost_count
            });
        }
        status.apply_restrictions(c.default_permissions, is_booster, false)
    }

    /// `set_chat_description` (ChatManager.cpp:3165-3176) — the
    /// EditChatAboutQuery send is [blocked: net] (M.5.45).
    pub fn set_chat_description(
        &mut self,
        _user_manager: &mut UserManager,
        env: &mut UserEnv,
        chat_id: ChatId,
        description: &str,
        mut promise: Promise<()>,
    ) {
        let _new_description = crate::misc::strip_empty_characters(description, 255, false);
        let Some(c) = self.get_chat(chat_id) else {
            return promise.set_error(Status::error(400, "Chat info not found"));
        };
        let permissions = Self::get_chat_permissions_record(c);
        let permissions =
            permissions.apply_restrictions(c.default_permissions, false, env.auth_is_bot);
        if !permissions.can_change_info_and_settings() {
            return promise.set_error(Status::error(
                400,
                "Not enough rights to set chat description",
            ));
        }
        tdutils::log_info!("EditChatAboutQuery for {chat_id:?} — [blocked: net]");
        promise.set_error(Status::error(500, "Network is not implemented yet"));
    }

    /// `set_channel_description` (ChatManager.cpp:3678-3688).
    pub fn set_channel_description(
        &mut self,
        _user_manager: &mut UserManager,
        env: &mut UserEnv,
        channel_id: ChannelId,
        description: &str,
        mut promise: Promise<()>,
    ) {
        let _new_description = crate::misc::strip_empty_characters(description, 255, false);
        let Some(c) = self.get_channel(channel_id) else {
            return promise.set_error(Status::error(400, "Chat info not found"));
        };
        let permissions = self.get_channel_permissions_channel(env.global, channel_id, c);
        if !permissions.can_change_info_and_settings() {
            return promise.set_error(Status::error(
                400,
                "Not enough rights to set chat description",
            ));
        }
        tdutils::log_info!("EditChatAboutQuery for {channel_id:?} — [blocked: net]");
        promise.set_error(Status::error(500, "Network is not implemented yet"));
    }

    /// `set_channel_username` (ChatManager.cpp:3178-3191) — the
    /// UpdateChannelUsernameQuery send is [blocked: net] (M.5.45).
    pub fn set_channel_username(
        &mut self,
        _user_manager: &mut UserManager,
        _env: &mut UserEnv,
        channel_id: ChannelId,
        username: &str,
        mut promise: Promise<()>,
    ) {
        let Some(c) = self.get_channel(channel_id) else {
            return promise.set_error(Status::error(400, "Supergroup not found"));
        };
        if !c.status.is_creator() {
            return promise.set_error(Status::error(
                400,
                "Not enough rights to change supergroup username",
            ));
        }
        if !username.is_empty() && !crate::misc::is_allowed_username(username) {
            return promise.set_error(Status::error(400, "Username is invalid"));
        }
        tdutils::log_info!("UpdateChannelUsernameQuery for {channel_id:?} — [blocked: net]");
        promise.set_error(Status::error(500, "Network is not implemented yet"));
    }

    /// `toggle_channel_username_is_active` (ChatManager.cpp:3194-3208) —
    /// the optimistic local update lands before the blocked-net query.
    pub fn toggle_channel_username_is_active(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        channel_id: ChannelId,
        username: String,
        is_active: bool,
        mut promise: Promise<()>,
    ) {
        let Some(c) = self.get_channel(channel_id) else {
            return promise.set_error(Status::error(400, "Supergroup not found"));
        };
        if !c.status.is_creator() {
            return promise.set_error(Status::error(400, "Not enough rights to change username"));
        }
        if !c.usernames.can_toggle(false, &username) {
            return promise.set_error(Status::error(400, "Wrong username specified"));
        }
        let toggled = c.usernames.toggle(false, &username, is_active);
        self.on_update_channel_usernames(user_manager, env, channel_id, toggled);
        self.update_channel(user_manager, env, channel_id, false, false);
        tdutils::log_info!("ToggleChannelUsernameQuery for {channel_id:?} — [blocked: net]");
        promise.set_error(Status::error(500, "Network is not implemented yet"));
    }

    /// `disable_all_channel_usernames` (ChatManager.cpp:3211-3221).
    pub fn disable_all_channel_usernames(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        channel_id: ChannelId,
        mut promise: Promise<()>,
    ) {
        let Some(c) = self.get_channel(channel_id) else {
            return promise.set_error(Status::error(400, "Supergroup not found"));
        };
        if !c.status.is_creator() {
            return promise.set_error(Status::error(400, "Not enough rights to disable usernames"));
        }
        let deactivated = c.usernames.deactivate_all();
        self.on_update_channel_usernames(user_manager, env, channel_id, deactivated);
        self.update_channel(user_manager, env, channel_id, false, false);
        tdutils::log_info!(
            "DeactivateAllChannelUsernamesQuery for {channel_id:?} — [blocked: net]"
        );
        promise.set_error(Status::error(500, "Network is not implemented yet"));
    }

    /// `reorder_channel_usernames` (ChatManager.cpp:3224-3240) — order
    /// lists of one or zero resolve locally without a query.
    pub fn reorder_channel_usernames(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        channel_id: ChannelId,
        usernames: Vec<String>,
        mut promise: Promise<()>,
    ) {
        let Some(c) = self.get_channel(channel_id) else {
            return promise.set_error(Status::error(400, "Supergroup not found"));
        };
        if !c.status.is_creator() {
            return promise.set_error(Status::error(400, "Not enough rights to reorder usernames"));
        }
        if !c.usernames.can_reorder_to(&usernames) {
            return promise.set_error(Status::error(400, "Invalid username order specified"));
        }
        if usernames.len() <= 1 {
            return promise.set_value(());
        }
        let reordered = c.usernames.reorder_to(usernames);
        self.on_update_channel_usernames(user_manager, env, channel_id, reordered);
        self.update_channel(user_manager, env, channel_id, false, false);
        tdutils::log_info!("ReorderChannelUsernamesQuery for {channel_id:?} — [blocked: net]");
        promise.set_error(Status::error(500, "Network is not implemented yet"));
    }

    /// `set_channel_accent_color` (ChatManager.cpp:3275-3293).
    pub fn set_channel_accent_color(
        &mut self,
        _user_manager: &mut UserManager,
        env: &mut UserEnv,
        channel_id: ChannelId,
        accent_color_id: AccentColorId,
        _background_custom_emoji_id: CustomEmojiId,
        mut promise: Promise<()>,
    ) {
        if !accent_color_id.is_valid() {
            return promise.set_error(Status::error(
                400,
                "Invalid accent color identifier specified",
            ));
        }
        let Some(c) = self.get_channel(channel_id) else {
            return promise.set_error(Status::error(400, "Chat not found"));
        };
        if c.is_megagroup {
            return promise.set_error(Status::error(
                400,
                "Accent color can be changed only in channel chats",
            ));
        }
        if !self
            .get_channel_permissions_channel(env.global, channel_id, c)
            .can_change_info_and_settings_as_administrator()
        {
            return promise.set_error(Status::error(400, "Not enough rights in the channel"));
        }
        tdutils::log_info!("UpdateChannelColorQuery for {channel_id:?} — [blocked: net]");
        promise.set_error(Status::error(500, "Network is not implemented yet"));
    }

    /// `set_channel_profile_accent_color` (ChatManager.cpp:3296-3308).
    pub fn set_channel_profile_accent_color(
        &mut self,
        _user_manager: &mut UserManager,
        env: &mut UserEnv,
        channel_id: ChannelId,
        _profile_accent_color_id: AccentColorId,
        _profile_background_custom_emoji_id: CustomEmojiId,
        mut promise: Promise<()>,
    ) {
        let Some(c) = self.get_channel(channel_id) else {
            return promise.set_error(Status::error(400, "Chat not found"));
        };
        if !self
            .get_channel_permissions_channel(env.global, channel_id, c)
            .can_change_info_and_settings_as_administrator()
        {
            return promise.set_error(Status::error(400, "Not enough rights in the chat"));
        }
        tdutils::log_info!("UpdateChannelColorQuery for {channel_id:?} — [blocked: net]");
        promise.set_error(Status::error(500, "Network is not implemented yet"));
    }

    /// `set_channel_emoji_status` (ChatManager.cpp:3311-3325).
    pub fn set_channel_emoji_status(
        &mut self,
        _user_manager: &mut UserManager,
        env: &mut UserEnv,
        channel_id: ChannelId,
        _emoji_status: Option<EmojiStatus>,
        mut promise: Promise<()>,
    ) {
        let Some(c) = self.get_channel(channel_id) else {
            return promise.set_error(Status::error(400, "Chat not found"));
        };
        if !self
            .get_channel_permissions_channel(env.global, channel_id, c)
            .can_change_info_and_settings_as_administrator()
        {
            return promise.set_error(Status::error(400, "Not enough rights in the chat"));
        }
        // StickersManager add_recent_emoji_status — the wave-2 stub.
        tdutils::log_info!("UpdateChannelEmojiStatusQuery for {channel_id:?} — [blocked: net]");
        promise.set_error(Status::error(500, "Network is not implemented yet"));
    }

    /// `set_channel_sticker_set` (ChatManager.cpp:3328-3356).
    pub fn set_channel_sticker_set(
        &mut self,
        _user_manager: &mut UserManager,
        env: &mut UserEnv,
        channel_id: ChannelId,
        sticker_set_id: StickerSetId,
        mut promise: Promise<()>,
    ) {
        let Some(c) = self.get_channel(channel_id) else {
            return promise.set_error(Status::error(400, "Supergroup not found"));
        };
        if !c.is_megagroup {
            return promise.set_error(Status::error(
                400,
                "Chat sticker set can be set only for supergroups",
            ));
        }
        if !self
            .get_channel_permissions_channel(env.global, channel_id, c)
            .can_change_info_and_settings_as_administrator()
        {
            return promise.set_error(Status::error(
                400,
                "Not enough rights to change supergroup sticker set",
            ));
        }
        if sticker_set_id.is_valid() {
            // stickers_manager_->get_input_sticker_set — the StickersManager
            // fan-out lands with wave 2; interim answers nothing to forward
            return promise.set_error(Status::error(400, "Sticker set not found"));
        }
        if let Some(f) = self.get_channel_full_const(channel_id)
            && !f.can_set_sticker_set
        {
            return promise.set_error(Status::error(400, "Can't set supergroup sticker set"));
        }
        tdutils::log_info!("SetChannelStickerSetQuery for {channel_id:?} — [blocked: net]");
        promise.set_error(Status::error(500, "Network is not implemented yet"));
    }

    /// `set_channel_emoji_sticker_set` (ChatManager.cpp:3359-3383) — the
    /// "Cuctom" typo is load-bearing (verbatim C++ string).
    pub fn set_channel_emoji_sticker_set(
        &mut self,
        _user_manager: &mut UserManager,
        env: &mut UserEnv,
        channel_id: ChannelId,
        sticker_set_id: StickerSetId,
        mut promise: Promise<()>,
    ) {
        let Some(c) = self.get_channel(channel_id) else {
            return promise.set_error(Status::error(400, "Supergroup not found"));
        };
        if !c.is_megagroup {
            return promise.set_error(Status::error(
                400,
                "Cuctom emoji sticker set can be set only for supergroups",
            ));
        }
        if !self
            .get_channel_permissions_channel(env.global, channel_id, c)
            .can_change_info_and_settings_as_administrator()
        {
            return promise.set_error(Status::error(
                400,
                "Not enough rights to change custom emoji sticker set in the supergroup",
            ));
        }
        if sticker_set_id.is_valid() {
            return promise.set_error(Status::error(400, "Sticker set not found"));
        }
        tdutils::log_info!("SetChannelEmojiStickerSetQuery for {channel_id:?} — [blocked: net]");
        promise.set_error(Status::error(500, "Network is not implemented yet"));
    }

    /// `set_channel_unrestrict_boost_count` (ChatManager.cpp:3386-3403).
    pub fn set_channel_unrestrict_boost_count(
        &mut self,
        _user_manager: &mut UserManager,
        _env: &mut UserEnv,
        channel_id: ChannelId,
        unrestrict_boost_count: i32,
        mut promise: Promise<()>,
    ) {
        let Some(c) = self.get_channel(channel_id) else {
            return promise.set_error(Status::error(400, "Supergroup not found"));
        };
        if !c.is_megagroup {
            return promise.set_error(Status::error(
                400,
                "Unrestrict boost count can be set only for supergroups",
            ));
        }
        if !self.get_channel_status(channel_id).can_restrict_members() {
            return promise.set_error(Status::error(
                400,
                "Not enough rights to change unrestrict boost count set in the supergroup",
            ));
        }
        if !(0..=8).contains(&unrestrict_boost_count) {
            return promise.set_error(Status::error(
                400,
                "Invalid new value for the unrestrict boost count specified",
            ));
        }
        tdutils::log_info!(
            "SetChannelBoostsToUnblockRestrictionsQuery for {channel_id:?} — [blocked: net]"
        );
        promise.set_error(Status::error(500, "Network is not implemented yet"));
    }

    /// `set_channel_main_profile_tab` (ChatManager.cpp:3406-3430) — the
    /// SetChannelMainProfileTabQuery send is [blocked: net]; its on_ok
    /// completion (`on_set_channel_main_profile_tab`) still lands.
    pub fn set_channel_main_profile_tab(
        &mut self,
        _user_manager: &mut UserManager,
        _env: &mut UserEnv,
        channel_id: ChannelId,
        main_profile_tab: td_api::ProfileTab,
        mut promise: Promise<()>,
    ) {
        let Some(c) = self.get_channel(channel_id) else {
            return promise.set_error(Status::error(400, "Supergroup not found"));
        };
        if Self::get_channel_type(c) == ChannelType::Megagroup {
            return promise.set_error(Status::error(
                400,
                "Main profile tab can't be changed in supergroups",
            ));
        }
        if !c.status.can_change_info_and_settings_as_administrator() {
            return promise.set_error(Status::error(
                400,
                "Not enough rights to change main profile tab",
            ));
        }
        let channel_type = Self::get_channel_type(c);
        let Some(profile_tab) = get_profile_tab_tdapi(Some(&main_profile_tab), channel_type) else {
            return promise.set_error(Status::error(400, "Profile tab must be non-empty"));
        };
        if !is_allowed_profile_tab(profile_tab, channel_type) {
            return promise.set_error(Status::error(
                400,
                "Invalid profile tab specified for the chat",
            ));
        }
        tdutils::log_info!("SetChannelMainProfileTabQuery for {channel_id:?} — [blocked: net]");
        promise.set_error(Status::error(500, "Network is not implemented yet"));
    }

    /// `on_set_channel_main_profile_tab` (ChatManager.cpp:3433-3441) — the
    /// query completion hook. Production caller: the net step (M.5.45).
    #[allow(dead_code)]
    fn on_set_channel_main_profile_tab(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        channel_id: ChannelId,
        main_profile_tab: ProfileTab,
        mut promise: Promise<()>,
    ) {
        if let Some(f) = self.get_channel_full_mut(channel_id)
            && f.main_profile_tab != main_profile_tab
        {
            f.main_profile_tab = main_profile_tab;
            f.is_changed = true;
            self.update_channel_full(
                user_manager,
                env,
                channel_id,
                "on_set_channel_main_profile_tab",
                false,
            );
        }
        promise.set_value(());
    }

    /// `delete_chat` (ChatManager.cpp:4021-4032) — the DeleteChatQuery
    /// send is [blocked: net] (M.5.46); local state mutation happens in
    /// the query's on_ok.
    pub fn delete_chat(&mut self, _env: &mut UserEnv, chat_id: ChatId, mut promise: Promise<()>) {
        let Some(c) = self.get_chat(chat_id) else {
            return promise.set_error(Status::error(400, "Chat info not found"));
        };
        if !Self::get_chat_permissions_record(c).is_creator() {
            return promise.set_error(Status::error(400, "Not enough rights to delete the chat"));
        }
        if !c.is_active {
            return promise.set_error(Status::error(400, "Chat is already deactivated"));
        }
        tdutils::log_info!("DeleteChatQuery for {chat_id:?} — [blocked: net]");
        promise.set_error(Status::error(500, "Network is not implemented yet"));
    }

    /// `delete_channel` (ChatManager.cpp:4036-4045) — the DeleteChannelQuery
    /// send is [blocked: net] (M.5.46).
    pub fn delete_channel(
        &mut self,
        _env: &mut UserEnv,
        channel_id: ChannelId,
        mut promise: Promise<()>,
    ) {
        let Some(c) = self.get_channel(channel_id) else {
            return promise.set_error(Status::error(400, "Chat info not found"));
        };
        if !c.can_be_deleted {
            return promise.set_error(Status::error(400, "The chat can't be deleted"));
        }
        tdutils::log_info!("DeleteChannelQuery for {channel_id:?} — [blocked: net]");
        promise.set_error(Status::error(500, "Network is not implemented yet"));
    }

    // ---- M.5.41: the toggle family (ChatManager.cpp:3444-3675) ----

    /// `toggle_channel_sign_messages` (ChatManager.cpp:3444-3458).
    pub fn toggle_channel_sign_messages(
        &mut self,
        env: &mut UserEnv,
        channel_id: ChannelId,
        _sign_messages: bool,
        _show_message_sender: bool,
        mut promise: Promise<()>,
    ) {
        let Some(c) = self.get_channel(channel_id) else {
            return promise.set_error(Status::error(400, "Supergroup not found"));
        };
        if Self::get_channel_type(c) == ChannelType::Megagroup {
            return promise.set_error(Status::error(
                400,
                "Message signatures can't be toggled in supergroups",
            ));
        }
        if !self
            .get_channel_permissions_channel(env.global, channel_id, c)
            .can_change_info_and_settings()
        {
            return promise.set_error(Status::error(
                400,
                "Not enough rights to toggle channel sign messages",
            ));
        }
        tdutils::log_info!("ToggleChannelSignaturesQuery for {channel_id:?} — [blocked: net]");
        promise.set_error(Status::error(500, "Network is not implemented yet"));
    }

    /// `toggle_channel_join_to_send` (ChatManager.cpp:3461-3473).
    pub fn toggle_channel_join_to_send(
        &mut self,
        _env: &mut UserEnv,
        channel_id: ChannelId,
        _join_to_send: bool,
        mut promise: Promise<()>,
    ) {
        let Some(c) = self.get_channel(channel_id) else {
            return promise.set_error(Status::error(400, "Supergroup not found"));
        };
        if Self::get_channel_type(c) == ChannelType::Broadcast || c.is_gigagroup || c.is_monoforum {
            return promise.set_error(Status::error(
                400,
                "The method can be called only for ordinary supergroups",
            ));
        }
        if !self.get_channel_status(channel_id).can_restrict_members() {
            return promise.set_error(Status::error(400, "Not enough rights"));
        }
        tdutils::log_info!("ToggleChannelJoinToSendQuery for {channel_id:?} — [blocked: net]");
        promise.set_error(Status::error(500, "Network is not implemented yet"));
    }

    /// `toggle_channel_join_request` (ChatManager.cpp:3476-3494).
    #[allow(clippy::too_many_arguments)]
    pub fn toggle_channel_join_request(
        &mut self,
        _user_manager: &mut UserManager,
        _env: &mut UserEnv,
        channel_id: ChannelId,
        join_request: bool,
        guard_bot_user_id: UserId,
        apply_to_invite_links: bool,
        mut promise: Promise<()>,
    ) {
        let Some(c) = self.get_channel(channel_id) else {
            return promise.set_error(Status::error(400, "Supergroup not found"));
        };
        if Self::get_channel_type(c) == ChannelType::Broadcast || c.is_gigagroup || c.is_monoforum {
            return promise.set_error(Status::error(
                400,
                "The method can be called only for ordinary supergroups",
            ));
        }
        if !self.get_channel_status(channel_id).can_restrict_members() {
            return promise.set_error(Status::error(400, "Not enough rights"));
        }
        let _ = (join_request, guard_bot_user_id, apply_to_invite_links);
        // the guard-bot input_user resolution is the net step's job (M.5.45)
        tdutils::log_info!("ToggleChannelJoinRequestQuery for {channel_id:?} — [blocked: net]");
        promise.set_error(Status::error(500, "Network is not implemented yet"));
    }

    /// `toggle_channel_is_all_history_available`
    /// (ChatManager.cpp:3497-3517) — the gate order is load-bearing.
    pub fn toggle_channel_is_all_history_available(
        &mut self,
        env: &mut UserEnv,
        channel_id: ChannelId,
        is_all_history_available: bool,
        mut promise: Promise<()>,
    ) {
        let Some(c) = self.get_channel(channel_id) else {
            return promise.set_error(Status::error(400, "Supergroup not found"));
        };
        if !self
            .get_channel_permissions_channel(env.global, channel_id, c)
            .can_change_info_and_settings()
        {
            return promise.set_error(Status::error(
                400,
                "Not enough rights to toggle all supergroup history availability",
            ));
        }
        if Self::get_channel_type(c) != ChannelType::Megagroup {
            return promise.set_error(Status::error(
                400,
                "Message history can be hidden in supergroups only",
            ));
        }
        if (c.is_forum || c.is_monoforum) && !is_all_history_available {
            return promise.set_error(Status::error(
                400,
                "Message history can't be hidden in forum and channel direct messages supergroups",
            ));
        }
        if c.has_linked_channel && !is_all_history_available {
            return promise.set_error(Status::error(
                400,
                "Message history can't be hidden in discussion supergroups",
            ));
        }
        tdutils::log_info!("TogglePrehistoryHiddenQuery for {channel_id:?} — [blocked: net]");
        promise.set_error(Status::error(500, "Network is not implemented yet"));
    }

    /// `toggle_channel_can_have_sponsored_messages`
    /// (ChatManager.cpp:3520-3534).
    pub fn toggle_channel_can_have_sponsored_messages(
        &mut self,
        _env: &mut UserEnv,
        channel_id: ChannelId,
        _can_have_sponsored_messages: bool,
        mut promise: Promise<()>,
    ) {
        let Some(c) = self.get_channel(channel_id) else {
            return promise.set_error(Status::error(400, "Supergroup not found"));
        };
        if !self.get_channel_status(channel_id).is_creator() {
            return promise.set_error(Status::error(
                400,
                "Not enough rights to disable sponsored messages",
            ));
        }
        if Self::get_channel_type(c) != ChannelType::Broadcast {
            return promise.set_error(Status::error(
                400,
                "Sponsored messages can be disabled only in channels",
            ));
        }
        tdutils::log_info!("RestrictSponsoredMessagesQuery for {channel_id:?} — [blocked: net]");
        promise.set_error(Status::error(500, "Network is not implemented yet"));
    }

    /// `toggle_channel_has_automatic_translation`
    /// (ChatManager.cpp:3537-3550).
    pub fn toggle_channel_has_automatic_translation(
        &mut self,
        _env: &mut UserEnv,
        channel_id: ChannelId,
        _has_automatic_translation: bool,
        mut promise: Promise<()>,
    ) {
        let Some(c) = self.get_channel(channel_id) else {
            return promise.set_error(Status::error(400, "Supergroup not found"));
        };
        if !c.status.can_change_info_and_settings_as_administrator() {
            return promise.set_error(Status::error(
                400,
                "Not enough rights to change automatic translation",
            ));
        }
        if Self::get_channel_type(c) != ChannelType::Broadcast {
            return promise.set_error(Status::error(
                400,
                "Automatic translation can be enabled only in channels",
            ));
        }
        tdutils::log_info!("ToggleAutotranslationQuery for {channel_id:?} — [blocked: net]");
        promise.set_error(Status::error(500, "Network is not implemented yet"));
    }

    /// `toggle_channel_has_hidden_participants`
    /// (ChatManager.cpp:3588-3593).
    pub fn toggle_channel_has_hidden_participants(
        &mut self,
        _user_manager: &mut UserManager,
        env: &mut UserEnv,
        channel_id: ChannelId,
        _has_hidden_participants: bool,
        // user_manager is consumed by get_channel_full_force above
        mut promise: Promise<()>,
    ) {
        let full = self.get_channel_full_const(channel_id).cloned();
        let check = self.can_hide_channel_participants(env, channel_id, full.as_ref());
        match check {
            tdutils::status::Result::Ok(()) => {
                tdutils::log_info!(
                    "ToggleParticipantsHiddenQuery for {channel_id:?} — [blocked: net]"
                );
                promise.set_error(Status::error(500, "Network is not implemented yet"));
            }
            tdutils::status::Result::Err(status) => promise.set_error(status),
        }
    }

    /// `toggle_channel_has_aggressive_anti_spam_enabled`
    /// (ChatManager.cpp:3639-3645).
    pub fn toggle_channel_has_aggressive_anti_spam_enabled(
        &mut self,
        _user_manager: &mut UserManager,
        env: &mut UserEnv,
        channel_id: ChannelId,
        _has_aggressive_anti_spam_enabled: bool,
        mut promise: Promise<()>,
    ) {
        let full = self.get_channel_full_const(channel_id).cloned();
        let check = self.can_toggle_channel_aggressive_anti_spam(env, channel_id, full.as_ref());
        match check {
            tdutils::status::Result::Ok(()) => {
                tdutils::log_info!("ToggleAntiSpamQuery for {channel_id:?} — [blocked: net]");
                promise.set_error(Status::error(500, "Network is not implemented yet"));
            }
            tdutils::status::Result::Err(status) => promise.set_error(status),
        }
    }

    /// `toggle_channel_is_forum` (ChatManager.cpp:3648-3664) — the no-op
    /// equal-state early-OK resolves without a query.
    pub fn toggle_channel_is_forum(
        &mut self,
        _env: &mut UserEnv,
        channel_id: ChannelId,
        is_forum: bool,
        is_forum_tabs: bool,
        mut promise: Promise<()>,
    ) {
        let Some(c) = self.get_channel(channel_id) else {
            return promise.set_error(Status::error(400, "Supergroup not found"));
        };
        if c.is_forum == is_forum && c.is_forum_tabs == is_forum_tabs {
            return promise.set_value(());
        }
        if !c.status.is_creator() {
            return promise.set_error(Status::error(
                400,
                "Not enough rights to convert the group to a forum",
            ));
        }
        if Self::get_channel_type(c) != ChannelType::Megagroup {
            return promise.set_error(Status::error(
                400,
                "Forums can be enabled in supergroups only",
            ));
        }
        tdutils::log_info!("ToggleForumQuery for {channel_id:?} — [blocked: net]");
        promise.set_error(Status::error(500, "Network is not implemented yet"));
    }

    /// `can_convert_channel_to_gigagroup` (ChatManager.cpp:3925-3928).
    pub fn can_convert_channel_to_gigagroup(
        &mut self,
        _env: &mut UserEnv,
        channel_id: ChannelId,
    ) -> bool {
        let Some(c) = self.get_channel(channel_id) else {
            return false;
        };
        if Self::get_channel_type(c) != ChannelType::Megagroup
            || !self.get_channel_status(channel_id).is_creator()
            || c.is_gigagroup
            || c.default_permissions != RestrictedRights::restrict_all()
        {
            return false;
        }
        true
    }

    /// `convert_channel_to_gigagroup` (ChatManager.cpp:3667-3675).
    pub fn convert_channel_to_gigagroup(
        &mut self,
        _user_manager: &mut UserManager,
        env: &mut UserEnv,
        channel_id: ChannelId,
        mut promise: Promise<()>,
    ) {
        if !self.can_convert_channel_to_gigagroup(env, channel_id) {
            return promise.set_error(Status::error(
                400,
                "Can't convert the chat to a broadcast group",
            ));
        }
        // td_->suggested_action_manager_->remove_dialog_suggested_action —
        // the M.13 stub.
        tdutils::log_info!("ConvertToGigagroupQuery for {channel_id:?} — [blocked: net]");
        promise.set_error(Status::error(500, "Network is not implemented yet"));
    }

    // ---- M.5.42: discussion / monoforum / location / slow mode /
    // statistics / report / paid messages (ChatManager.cpp:3691-4018) ----

    /// `set_channel_discussion_group` (ChatManager.cpp:3691-3758). The
    /// have_dialog_force gates are the DialogManager stubs (M.6/M.10); the
    /// SetDiscussionGroupQuery send is [blocked: net].
    pub fn set_channel_discussion_group(
        &mut self,
        _user_manager: &mut UserManager,
        _env: &mut UserEnv,
        dialog_id: DialogId,
        discussion_dialog_id: DialogId,
        mut promise: Promise<()>,
    ) {
        if !dialog_id.is_valid() && !discussion_dialog_id.is_valid() {
            return promise.set_error(Status::error(400, "Invalid chat identifiers specified"));
        }
        // the broadcast side
        if dialog_id.is_valid() {
            // have_dialog_force — the DialogManager stub (M.6)
            if self.get_channel_type_by_id(dialog_id.get_channel_id()) != ChannelType::Broadcast {
                return promise.set_error(Status::error(400, "Chat is not a channel"));
            }
            let Some(c) = self.get_channel(dialog_id.get_channel_id()) else {
                return promise.set_error(Status::error(400, "Chat info not found"));
            };
            if c.is_megagroup {
                return promise.set_error(Status::error(400, "Chat is not a channel"));
            }
            if !c.status.can_change_info_and_settings_as_administrator() {
                return promise.set_error(Status::error(400, "Not enough rights in the channel"));
            }
            // get_input_channel — CHECKed non-null in C++
        }
        // the discussion (megagroup) side
        if discussion_dialog_id.is_valid() {
            if self.get_channel_type_by_id(discussion_dialog_id.get_channel_id())
                != ChannelType::Megagroup
            {
                return promise
                    .set_error(Status::error(400, "Discussion chat is not a supergroup"));
            }
            let Some(c) = self.get_channel(discussion_dialog_id.get_channel_id()) else {
                return promise.set_error(Status::error(400, "Discussion chat info not found"));
            };
            if !c.is_megagroup {
                return promise
                    .set_error(Status::error(400, "Discussion chat is not a supergroup"));
            }
            if !(c.status.is_administrator() && c.status.can_pin_messages()) {
                return promise
                    .set_error(Status::error(400, "Not enough rights in the supergroup"));
            }
        }
        tdutils::log_info!(
            "SetDiscussionGroupQuery for {dialog_id:?}/{discussion_dialog_id:?} — [blocked: net]"
        );
        promise.set_error(Status::error(500, "Network is not implemented yet"));
    }

    /// `set_channel_monoforum_group` (ChatManager.cpp:3761-3792).
    pub fn set_channel_monoforum_group(
        &mut self,
        _user_manager: &mut UserManager,
        _env: &mut UserEnv,
        dialog_id: DialogId,
        is_enabled: bool,
        mut paid_message_star_count: i64,
        mut promise: Promise<()>,
    ) {
        if !dialog_id.is_valid() {
            return promise.set_error(Status::error(400, "Invalid chat identifier specified"));
        }
        // have_dialog_force — the DialogManager stub (M.6)
        if self.get_channel_type_by_id(dialog_id.get_channel_id()) != ChannelType::Broadcast {
            return promise.set_error(Status::error(400, "Chat is not a supergroup"));
        }
        let Some(c) = self.get_channel(dialog_id.get_channel_id()) else {
            return promise.set_error(Status::error(400, "Chat info not found"));
        };
        if c.is_megagroup {
            return promise.set_error(Status::error(400, "Chat is not a channel"));
        }
        if !c.status.is_creator() {
            return promise.set_error(Status::error(400, "Not enough rights in the channel"));
        }
        if !is_enabled {
            paid_message_star_count = 0;
            let _ = paid_message_star_count;
        } else if !(0..=1_000_000).contains(&paid_message_star_count) {
            return promise.set_error(Status::error(
                400,
                "Invalid number of Telegram Stars specified",
            ));
        }
        tdutils::log_info!("UpdatePaidMessagesPriceQuery for {dialog_id:?} — [blocked: net]");
        promise.set_error(Status::error(500, "Network is not implemented yet"));
    }

    /// `set_channel_location` (ChatManager.cpp:3795-3811).
    pub fn set_channel_location(
        &mut self,
        _user_manager: &mut UserManager,
        _env: &mut UserEnv,
        channel_id: ChannelId,
        location: &DialogLocation,
        mut promise: Promise<()>,
    ) {
        if location.empty() {
            return promise.set_error(Status::error(400, "Invalid chat location specified"));
        }
        let Some(c) = self.get_channel(channel_id) else {
            return promise.set_error(Status::error(400, "Chat info not found"));
        };
        if !c.is_megagroup {
            return promise.set_error(Status::error(400, "Chat is not a supergroup"));
        }
        if !c.status.is_creator() {
            return promise.set_error(Status::error(400, "Not enough rights in the supergroup"));
        }
        tdutils::log_info!("EditLocationQuery for {channel_id:?} — [blocked: net]");
        promise.set_error(Status::error(500, "Network is not implemented yet"));
    }

    /// `set_channel_slow_mode_delay` (ChatManager.cpp:3814-3843) — the
    /// allowed-value check precedes the id validity check, like the C++.
    pub fn set_channel_slow_mode_delay(
        &mut self,
        _user_manager: &mut UserManager,
        _env: &mut UserEnv,
        dialog_id: DialogId,
        slow_mode_delay: i32,
        mut promise: Promise<()>,
    ) {
        if !matches!(slow_mode_delay, 0 | 5 | 10 | 30 | 60 | 300 | 900 | 3600) {
            return promise.set_error(Status::error(400, "Invalid new value for slow mode delay"));
        }
        if !dialog_id.is_valid() {
            return promise.set_error(Status::error(400, "Invalid chat identifier specified"));
        }
        if self.get_channel_type_by_id(dialog_id.get_channel_id()) != ChannelType::Megagroup {
            return promise.set_error(Status::error(400, "Chat is not a supergroup"));
        }
        let Some(c) = self.get_channel(dialog_id.get_channel_id()) else {
            return promise.set_error(Status::error(400, "Chat info not found"));
        };
        if !c.is_megagroup {
            return promise.set_error(Status::error(400, "Chat is not a supergroup"));
        }
        if !self
            .get_channel_status(dialog_id.get_channel_id())
            .can_restrict_members()
        {
            return promise.set_error(Status::error(400, "Not enough rights in the supergroup"));
        }
        tdutils::log_info!("ToggleSlowModeQuery for {dialog_id:?} — [blocked: net]");
        promise.set_error(Status::error(500, "Network is not implemented yet"));
    }

    /// `get_channel_statistics_dc_id` (ChatManager.cpp:3846-3876) — the
    /// full re-fetch on a missing/unexact DC rides the blocked-net query.
    pub fn get_channel_statistics_dc_id(
        &mut self,
        _user_manager: &mut UserManager,
        env: &mut UserEnv,
        dialog_id: DialogId,
        for_full_statistics: bool,
        mut promise: Promise<DcId>,
    ) {
        if !dialog_id.is_valid() {
            return promise.set_error(Status::error(400, "Invalid chat identifier specified"));
        }
        if self.get_channel_type_by_id(dialog_id.get_channel_id()) != ChannelType::Broadcast {
            return promise.set_error(Status::error(400, "Chat is not a channel"));
        }
        let Some(_) = self.get_channel(dialog_id.get_channel_id()) else {
            return promise.set_error(Status::error(400, "Chat info not found"));
        };
        let dc = self
            .get_channel_full_const(dialog_id.get_channel_id())
            .map(|f| f.stats_dc_id);
        let can_view = self
            .get_channel_full_const(dialog_id.get_channel_id())
            .map(|f| f.can_view_statistics)
            .unwrap_or(false);
        if dc.is_none_or(|dc| !dc.is_exact()) || (for_full_statistics && !can_view) {
            self.send_get_channel_full_query(
                env,
                dialog_id.get_channel_id(),
                Promise::default(),
                "get_channel_statistics_dc_id",
            );
            return promise.set_error(Status::error(500, "Network is not implemented yet"));
        }
        promise.set_value(dc.unwrap_or_default());
    }

    /// `can_get_channel_message_statistics` (ChatManager.cpp:3895-3906).
    pub fn can_get_channel_message_statistics(&self, env: &UserEnv, channel_id: ChannelId) -> bool {
        debug_assert!(!env.auth_is_bot);
        let Some(c) = self.get_channel(channel_id) else {
            return false;
        };
        if c.is_megagroup {
            return false;
        }
        match self.get_channel_full_const(channel_id) {
            Some(f) => f.stats_dc_id.is_exact(),
            None => c.status.can_post_messages(),
        }
    }

    /// `can_get_channel_story_statistics` (ChatManager.cpp:3909-3921) — a
    /// verbatim duplicate of the message-statistics predicate in the C++.
    pub fn can_get_channel_story_statistics(&self, env: &UserEnv, channel_id: ChannelId) -> bool {
        debug_assert!(!env.auth_is_bot);
        let Some(c) = self.get_channel(channel_id) else {
            return false;
        };
        if c.is_megagroup {
            return false;
        }
        match self.get_channel_full_const(channel_id) {
            Some(f) => f.stats_dc_id.is_exact(),
            None => c.status.can_post_messages(),
        }
    }

    /// `report_channel_spam` (ChatManager.cpp:3931-3965). The per-message
    /// sender grouping needs MessagesManager (M.6); with no senders the
    /// C++ answers OK locally.
    pub fn report_channel_spam(
        &mut self,
        _user_manager: &mut UserManager,
        _env: &mut UserEnv,
        channel_id: ChannelId,
        _message_ids: &[MessageId],
        mut promise: Promise<()>,
    ) {
        let Some(c) = self.get_channel(channel_id) else {
            return promise.set_error(Status::error(400, "Supergroup not found"));
        };
        if !c.is_megagroup {
            return promise.set_error(Status::error(
                400,
                "Spam can be reported only in supergroups",
            ));
        }
        if !c.status.is_administrator() {
            return promise.set_error(Status::error(
                400,
                "Spam can be reported only by chat administrators",
            ));
        }
        // the per-message sender grouping is the M.6 stub; with no senders
        // resolvable locally the C++ reports nothing and answers OK
        promise.set_value(());
    }

    /// `report_channel_anti_spam_false_positive`
    /// (ChatManager.cpp:3969-3985).
    pub fn report_channel_anti_spam_false_positive(
        &mut self,
        _user_manager: &mut UserManager,
        _env: &mut UserEnv,
        channel_id: ChannelId,
        message_id: MessageId,
        mut promise: Promise<()>,
    ) {
        let Some(c) = self.get_channel(channel_id) else {
            return promise.set_error(Status::error(400, "Supergroup not found"));
        };
        if !c.is_megagroup {
            return promise.set_error(Status::error(400, "The chat is not a supergroup"));
        }
        if !c.status.is_administrator() {
            return promise.set_error(Status::error(
                400,
                "Anti-spam checks false positives can be reported only by chat administrators",
            ));
        }
        if !message_id.is_server() {
            return promise.set_error(Status::error(400, "Invalid message identifier specified"));
        }
        tdutils::log_info!(
            "ReportChannelAntiSpamFalsePositiveQuery for {channel_id:?} — [blocked: net]"
        );
        promise.set_error(Status::error(500, "Network is not implemented yet"));
    }

    /// `set_channel_send_paid_message_star_count`
    /// (ChatManager.cpp:3989-4018).
    pub fn set_channel_send_paid_message_star_count(
        &mut self,
        _user_manager: &mut UserManager,
        _env: &mut UserEnv,
        dialog_id: DialogId,
        count: i64,
        mut promise: Promise<()>,
    ) {
        if !dialog_id.is_valid() {
            return promise.set_error(Status::error(400, "Invalid chat identifier specified"));
        }
        if self.get_channel_type_by_id(dialog_id.get_channel_id()) != ChannelType::Megagroup {
            return promise.set_error(Status::error(400, "Chat is not a supergroup"));
        }
        let Some(c) = self.get_channel(dialog_id.get_channel_id()) else {
            return promise.set_error(Status::error(400, "Chat info not found"));
        };
        if !c.is_megagroup {
            return promise.set_error(Status::error(400, "Chat is not a supergroup"));
        }
        if !self
            .get_channel_status(dialog_id.get_channel_id())
            .can_restrict_members()
        {
            return promise.set_error(Status::error(400, "Not enough rights in the supergroup"));
        }
        if !(0..=1_000_000).contains(&count) {
            return promise.set_error(Status::error(
                400,
                "Invalid number of Telegram Stars specified",
            ));
        }
        tdutils::log_info!("UpdatePaidMessagesPriceQuery for {dialog_id:?} — [blocked: net]");
        promise.set_error(Status::error(500, "Network is not implemented yet"));
    }

    /// `ChatManager::save_chat` (ChatManager.cpp:4467-4484): the binlog
    /// mirror (`ChatLogEvent`, `HandlerType::Chats`) add/rewrite while the
    /// record is unsaved, then the sqlite row. Reached from
    /// [`ChatManager::update_chat`]'s need-save fan-out.
    fn save_chat(&mut self, env: &mut UserEnv, chat_id: ChatId, from_binlog: bool) {
        if !env.global.use_chat_info_database() {
            return;
        }
        let need_save = match self.chats.get(&chat_id.get()) {
            Some(c) => !c.is_saved,
            None => return,
        };
        if need_save {
            if !from_binlog {
                let data = {
                    let c = self.chats.get(&chat_id.get()).unwrap();
                    let mut data = Vec::new();
                    store_chat_log_event(&mut data, chat_id, c);
                    data
                };
                let binlog = env.global.td_db().binlog();
                let log_event_id = self.chats.get(&chat_id.get()).unwrap().log_event_id;
                let new_id = if log_event_id == 0 {
                    binlog.add(tddb::td_db::LOG_EVENT_HANDLER_TYPE_CHATS, &data)
                } else {
                    binlog.rewrite(
                        log_event_id,
                        tddb::td_db::LOG_EVENT_HANDLER_TYPE_CHATS,
                        &data,
                    )
                };
                self.chats.get_mut(&chat_id.get()).unwrap().log_event_id = new_id;
            }
            self.save_chat_to_database(env, chat_id);
        }
    }

    /// `save_chat_to_database` + `_impl` + `on_save_chat_to_database`
    /// (ChatManager.cpp:4526-4580) collapsed onto the synchronous kv — the
    /// C++ guards exist to order against the async pmc and cannot
    /// interleave here; the write is infallible, so the failure branch
    /// (`is_saved = false` + re-save) is unreachable.
    fn save_chat_to_database(&mut self, env: &mut UserEnv, chat_id: ChatId) {
        let value = {
            let Some(c) = self.chats.get_mut(&chat_id.get()) else {
                return;
            };
            if c.is_being_saved {
                return;
            }
            c.is_being_saved = true;
            c.is_saved = true;
            Self::get_chat_database_value(c)
        };
        env.global
            .td_db()
            .common_kv()
            .set(&Self::get_chat_database_key(chat_id), &value);
        let c = self.chats.get_mut(&chat_id.get()).unwrap();
        c.is_being_saved = false;
        // `on_save_chat_to_database`: the row is durable, retire the
        // binlog mirror (the row is durable).
        if c.log_event_id != 0 {
            let log_event_id = c.log_event_id;
            env.global.td_db().binlog().erase(log_event_id);
            c.log_event_id = 0;
        }
    }

    /// `load_chat_from_database` + `_impl` + `on_load_chat_from_database`
    /// (ChatManager.cpp:4582-4658) collapsed onto the synchronous kv (the
    /// per-id promise queue exists only to batch the async completions).
    fn load_chat_from_database(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        chat_id: ChatId,
        force: bool,
    ) {
        if env.global.close_flag() && !force {
            return;
        }
        debug_assert!(chat_id.is_valid());
        if !self.loaded_from_database_chats.insert(chat_id.get()) {
            return;
        }
        let value = env
            .global
            .td_db()
            .common_kv()
            .get(&Self::get_chat_database_key(chat_id));
        if let Some(c) = self.chats.get_mut(&chat_id.get()) {
            // The C++ races an in-memory chat against the async load; here
            // the only entry would be a binlog-replayed chat: keep the
            // newer memory state if it differs from the row, else retire
            // the mirror.
            debug_assert!(!c.is_saved && !c.is_being_saved);
            let new_value = Self::get_chat_database_value(c);
            if value != new_value {
                self.save_chat_to_database(env, chat_id);
            } else if c.log_event_id != 0 {
                let log_event_id = c.log_event_id;
                env.global.td_db().binlog().erase(log_event_id);
                c.log_event_id = 0;
            }
        } else if !value.is_empty() {
            match Chat::parse_value(&value) {
                Ok(chat) => {
                    let chat_id_value = chat_id.get();
                    self.chats.insert(chat_id_value, chat);
                    let c = self.chats.get_mut(&chat_id_value).unwrap();
                    c.is_saved = true;
                    self.update_chat(user_manager, env, chat_id, true, true);
                }
                Err(error) => {
                    tdutils::log_error!("Failed to load {chat_id:?} from database: {error}");
                    self.chats.remove(&chat_id.get());
                }
            }
        }

        let migrated_to_channel_id = self
            .chats
            .get(&chat_id.get())
            .map(|c| c.migrated_to_channel_id);
        if let Some(migrated_to_channel_id) = migrated_to_channel_id
            && migrated_to_channel_id.is_valid()
            && !self.have_channel_force(
                user_manager,
                env,
                migrated_to_channel_id,
                "on_load_chat_from_database",
            )
        {
            tdutils::log_error!("Can't find the migrated-to channel from {chat_id:?}");
        }
    }

    /// `ChatManager::on_binlog_chat_event` (ChatManager.cpp:4486-4506):
    /// the crash-window replay — restore the chat from its binlog mirror,
    /// keep the event id so the save rewrites (not duplicates) it, then run
    /// the C++ update path's save fan-out (from_binlog = true).
    pub fn on_binlog_chat_event(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        event: &tddb::binlog::event::BinlogEvent,
    ) {
        let binlog = env.global.td_db().binlog();
        if !env.global.use_chat_info_database() {
            binlog.erase(event.id);
            return;
        }

        let (chat_id, chat) = match parse_chat_log_event(event.get_data()) {
            Ok(parsed) => parsed,
            Err(error) => {
                tdutils::log_error!("Failed to load a basic group from binlog: {error}");
                binlog.erase(event.id);
                return;
            }
        };
        if self.have_chat(chat_id) || !chat_id.is_valid() {
            tdutils::log_error!("Skip adding already added {chat_id:?}");
            binlog.erase(event.id);
            return;
        }

        let chat_entry = self.add_chat(chat_id);
        *chat_entry = chat;
        self.chats.get_mut(&chat_id.get()).unwrap().log_event_id = event.id;

        // update_chat(c, chat_id, true, false) — its need-save fan-out is
        // save_chat(from_binlog = true).
        self.update_chat(user_manager, env, chat_id, true, false);
    }

    /// `have_chat_force` (ChatManager.cpp:4660-4662).
    pub fn have_chat_force(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        chat_id: ChatId,
        source: &str,
    ) -> bool {
        self.get_chat_force(user_manager, env, chat_id, source)
            .is_some()
    }

    /// `get_chat_force` (ChatManager.cpp:4664-4687) — the synchronous
    /// db-load path (the C++ caller-retry contract: `None` after an empty
    /// load means the query layer re-parks).
    pub fn get_chat_force(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        chat_id: ChatId,
        source: &str,
    ) -> Option<&mut Chat> {
        if !chat_id.is_valid() {
            return None;
        }
        if self.chats.contains_key(&chat_id.get()) {
            let migrated_to_channel_id = self
                .chats
                .get(&chat_id.get())
                .map(|c| c.migrated_to_channel_id);
            if let Some(migrated_to_channel_id) = migrated_to_channel_id
                && migrated_to_channel_id.is_valid()
                && !self.have_channel_force(user_manager, env, migrated_to_channel_id, source)
            {
                tdutils::log_error!(
                    "Can't find the migrated-to channel from {chat_id:?} from {source}"
                );
            }
            return self.chats.get_mut(&chat_id.get());
        }
        if !env.global.use_chat_info_database() {
            return None;
        }
        if self.loaded_from_database_chats.contains(&chat_id.get()) {
            return None;
        }

        self.load_chat_from_database(user_manager, env, chat_id, true);
        self.chats.get_mut(&chat_id.get())
    }

    /// `ChatManager::save_channel` (ChatManager.cpp:4713-4730) — the
    /// channel twin of [`ChatManager::save_chat`], reached from
    /// [`ChatManager::update_channel`]'s need-save fan-out.
    fn save_channel(&mut self, env: &mut UserEnv, channel_id: ChannelId, from_binlog: bool) {
        if !env.global.use_chat_info_database() {
            return;
        }
        let need_save = match self.channels.get(&channel_id.get()) {
            Some(c) => !c.is_saved,
            None => return,
        };
        if need_save {
            if !from_binlog {
                let data = {
                    let c = self.channels.get(&channel_id.get()).unwrap();
                    let ctx = env.time_ctx();
                    let mut data = Vec::new();
                    store_channel_log_event(&mut data, channel_id, c, &ctx);
                    data
                };
                let binlog = env.global.td_db().binlog();
                let log_event_id = self.channels.get(&channel_id.get()).unwrap().log_event_id;
                let new_id = if log_event_id == 0 {
                    binlog.add(tddb::td_db::LOG_EVENT_HANDLER_TYPE_CHANNELS, &data)
                } else {
                    binlog.rewrite(
                        log_event_id,
                        tddb::td_db::LOG_EVENT_HANDLER_TYPE_CHANNELS,
                        &data,
                    )
                };
                self.channels
                    .get_mut(&channel_id.get())
                    .unwrap()
                    .log_event_id = new_id;
            }
            self.save_channel_to_database(env, channel_id);
        }
    }

    /// `save_channel_to_database` + `_impl` + `on_save_channel_to_database`
    /// (ChatManager.cpp:4772-4826) collapsed onto the synchronous kv.
    fn save_channel_to_database(&mut self, env: &mut UserEnv, channel_id: ChannelId) {
        let value = {
            let Some(c) = self.channels.get_mut(&channel_id.get()) else {
                return;
            };
            if c.is_being_saved {
                return;
            }
            c.is_being_saved = true;
            c.is_saved = true;
            let ctx = env.time_ctx();
            Self::get_channel_database_value(c, &ctx)
        };
        env.global
            .td_db()
            .common_kv()
            .set(&Self::get_channel_database_key(channel_id), &value);
        let c = self.channels.get_mut(&channel_id.get()).unwrap();
        c.is_being_saved = false;
        if c.log_event_id != 0 {
            let log_event_id = c.log_event_id;
            env.global.td_db().binlog().erase(log_event_id);
            c.log_event_id = 0;
        }
    }

    /// `load_channel_from_database` + `_impl` +
    /// `on_load_channel_from_database` (ChatManager.cpp:4828-4957)
    /// collapsed onto the synchronous kv. The C++ async promise re-parking
    /// for the monoforum channel collapses into a synchronous recursive
    /// force-load. The in-memory/row merge fan-outs
    /// (`on_channel_status_changed`, `on_channel_usernames_changed`, the
    /// participant-count patch) land with M.5.13/M.5.30 — with the binlog
    /// mirror stubbed, the branch never sees an in-memory record.
    fn load_channel_from_database(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        channel_id: ChannelId,
        force: bool,
        is_recursive: bool,
    ) {
        if env.global.close_flag() && !force {
            return;
        }
        debug_assert!(channel_id.is_valid());
        if !self.loaded_from_database_channels.insert(channel_id.get()) {
            return;
        }
        let value = env
            .global
            .td_db()
            .common_kv()
            .get(&Self::get_channel_database_key(channel_id));
        if let Some(c) = self.channels.get_mut(&channel_id.get()) {
            debug_assert!(!c.is_saved && !c.is_being_saved);
            let ctx = env.time_ctx();
            let new_value = Self::get_channel_database_value(c, &ctx);
            if value != new_value {
                self.save_channel_to_database(env, channel_id);
            } else if c.log_event_id != 0 {
                let log_event_id = c.log_event_id;
                env.global.td_db().binlog().erase(log_event_id);
                c.log_event_id = 0;
            }
        } else if !value.is_empty() {
            let ctx = env.time_ctx();
            match Channel::parse_value(&value, &ctx) {
                Ok(channel) => {
                    let channel_id_value = channel_id.get();
                    self.channels.insert(channel_id_value, channel);
                    let c = self.channels.get_mut(&channel_id_value).unwrap();
                    c.is_saved = true;
                    self.update_channel(user_manager, env, channel_id, true, true);
                }
                Err(error) => {
                    tdutils::log_error!("Failed to load {channel_id:?} from database: {error}");
                    self.channels.remove(&channel_id.get());
                }
            }
        }

        let monoforum_channel_id = self
            .channels
            .get(&channel_id.get())
            .map(|c| c.monoforum_channel_id);
        if let Some(monoforum_channel_id) = monoforum_channel_id
            && monoforum_channel_id.is_valid()
            && !self.have_channel(monoforum_channel_id)
        {
            if is_recursive
                || self
                    .loaded_from_database_channels
                    .contains(&monoforum_channel_id.get())
            {
                // the C++ logs at INFO and gives up
            } else if self
                .get_channel_force(
                    user_manager,
                    env,
                    monoforum_channel_id,
                    "on_load_channel_from_database",
                    is_recursive,
                )
                .is_none()
            {
                // the C++ logs at INFO
            }
        }
    }

    /// `ChatManager::on_binlog_channel_event` (ChatManager.cpp:4732-4755):
    /// the channel twin of [`ChatManager::on_binlog_chat_event`].
    pub fn on_binlog_channel_event(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        event: &tddb::binlog::event::BinlogEvent,
    ) {
        let binlog = env.global.td_db().binlog();
        if !env.global.use_chat_info_database() {
            binlog.erase(event.id);
            return;
        }

        let ctx = env.time_ctx();
        let (channel_id, channel) = match parse_channel_log_event(event.get_data(), &ctx) {
            Ok(parsed) => parsed,
            Err(error) => {
                tdutils::log_error!("Failed to load a supergroup from binlog: {error}");
                binlog.erase(event.id);
                return;
            }
        };
        if self.have_channel(channel_id) || !channel_id.is_valid() {
            tdutils::log_error!("Skip adding already added {channel_id:?}");
            binlog.erase(event.id);
            return;
        }

        let channel_entry = self.add_channel(channel_id);
        *channel_entry = channel;
        self.channels
            .get_mut(&channel_id.get())
            .unwrap()
            .log_event_id = event.id;

        // update_channel(c, channel_id, true, false) — its need-save
        // fan-out is save_channel(from_binlog = true).
        self.update_channel(user_manager, env, channel_id, true, false);
    }

    /// `have_channel_force` (ChatManager.cpp:4959-4961).
    pub fn have_channel_force(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        channel_id: ChannelId,
        source: &str,
    ) -> bool {
        self.get_channel_force(user_manager, env, channel_id, source, false)
            .is_some()
    }

    /// `get_channel_force` (ChatManager.cpp:4963-4976).
    pub fn get_channel_force(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        channel_id: ChannelId,
        source: &str,
        is_recursive: bool,
    ) -> Option<&mut Channel> {
        let _ = source;
        if !channel_id.is_valid() {
            return None;
        }
        if self.channels.contains_key(&channel_id.get()) {
            return self.channels.get_mut(&channel_id.get());
        }
        if !env.global.use_chat_info_database() {
            return None;
        }
        if self
            .loaded_from_database_channels
            .contains(&channel_id.get())
        {
            return None;
        }

        self.load_channel_from_database(user_manager, env, channel_id, true, is_recursive);
        self.channels.get_mut(&channel_id.get())
    }

    // ---- M.5.14: the three member MultiTimeouts (ChatManager.h:994-996),
    // driven by the Td tick instead of the actor framework ----

    /// `MultiTimeout::set_timeout_in` (deadline = now + in; re-set replaces).
    fn set_timeout(deadlines: &mut BTreeMap<i64, f64>, channel_id: ChannelId, timeout_in: f64) {
        deadlines.insert(channel_id.get(), tdutils::time::now() + timeout_in);
    }

    /// `MultiTimeout::cancel_timeout`.
    fn cancel_timeout(deadlines: &mut BTreeMap<i64, f64>, channel_id: ChannelId) {
        deadlines.remove(&channel_id.get());
    }

    /// The earliest armed deadline across the three timeouts, or `None` —
    /// the Td actor's re-arm key.
    pub(crate) fn next_deadline(&self) -> Option<f64> {
        self.channel_emoji_status_timeouts
            .values()
            .chain(self.channel_unban_timeouts.values())
            .chain(self.slow_mode_delay_timeouts.values())
            .copied()
            .min_by(f64::total_cmp)
    }

    /// Fire every expired deadline (the `on_*_timeout_callback` +
    /// `send_closure_later` hop collapses into a direct call, the M.4.7
    /// convention); the caller re-arms from [`ChatManager::next_deadline`].
    pub fn tick_timeouts(&mut self, user_manager: &mut UserManager, env: &mut UserEnv) {
        let now = tdutils::time::now();
        let expired: Vec<i64> = self
            .channel_emoji_status_timeouts
            .iter()
            .filter(|(_, deadline)| **deadline <= now)
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            self.channel_emoji_status_timeouts.remove(&id);
            self.on_channel_emoji_status_timeout(user_manager, env, ChannelId::new(id));
        }
        let expired: Vec<i64> = self
            .channel_unban_timeouts
            .iter()
            .filter(|(_, deadline)| **deadline <= now)
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            self.channel_unban_timeouts.remove(&id);
            self.on_channel_unban_timeout(user_manager, env, ChannelId::new(id));
        }
        let expired: Vec<i64> = self
            .slow_mode_delay_timeouts
            .iter()
            .filter(|(_, deadline)| **deadline <= now)
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            self.slow_mode_delay_timeouts.remove(&id);
            self.on_slow_mode_delay_timeout(user_manager, env, ChannelId::new(id));
        }
    }

    /// `on_channel_emoji_status_timeout` (ChatManager.cpp:1762-1772).
    fn on_channel_emoji_status_timeout(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        channel_id: ChannelId,
    ) {
        if env.global.close_flag() {
            return;
        }
        if self.get_channel(channel_id).is_none() {
            debug_assert!(false, "Missing {channel_id:?} for the emoji status timeout");
            return;
        }
        debug_assert!(
            self.get_channel(channel_id)
                .is_some_and(|c| c.is_update_supergroup_sent)
        );
        self.update_channel(user_manager, env, channel_id, false, false);
    }

    /// `on_channel_unban_timeout` (ChatManager.cpp:1784-1805).
    fn on_channel_unban_timeout(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        channel_id: ChannelId,
    ) {
        if env.global.close_flag() {
            return;
        }
        {
            let Some(c) = self.get_channel_mut(channel_id) else {
                debug_assert!(false, "Missing {channel_id:?} for the unban timeout");
                return;
            };
            let old_status = c.status.clone();
            c.status.update_restrictions(env.global);
            if c.status == old_status {
                if c.status.is_restricted() || c.status.is_banned() {
                    tdutils::log_error!("Status of {channel_id:?} wasn't updated: {old_status:?}");
                }
            } else {
                c.is_changed = true;
            }
            tdutils::log_info!("Update {channel_id:?} status");
            c.is_status_changed = true;
        }
        let need_drop_slow_mode_delay = !self
            .get_channel(channel_id)
            .is_some_and(|c| c.is_slow_mode_enabled);
        self.invalidate_channel_full(
            user_manager,
            env,
            channel_id,
            need_drop_slow_mode_delay,
            "on_channel_unban_timeout",
        );
        // always call, because in case of failure we need to reactivate timeout
        self.update_channel(user_manager, env, channel_id, false, false);
    }

    /// `on_slow_mode_delay_timeout` (ChatManager.cpp:1817-1825).
    fn on_slow_mode_delay_timeout(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        channel_id: ChannelId,
    ) {
        if env.global.close_flag() {
            return;
        }
        self.on_update_channel_slow_mode_next_send_date(user_manager, env, channel_id, 0);
    }

    /// `on_update_channel_slow_mode_next_send_date`
    /// (ChatManager.cpp:8243-8253).
    pub(crate) fn on_update_channel_slow_mode_next_send_date(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        channel_id: ChannelId,
        slow_mode_next_send_date: i32,
    ) {
        let unchanged = self
            .channels_full
            .get(&channel_id.get())
            .is_none_or(|f| f.slow_mode_next_send_date == slow_mode_next_send_date);
        if unchanged {
            return;
        }
        {
            let Some(f) = self.channels_full.get_mut(&channel_id.get()) else {
                return;
            };
            f.slow_mode_next_send_date = slow_mode_next_send_date;
            f.is_slow_mode_next_send_date_changed = true;
            f.is_changed = true;
        }
        self.update_channel_full(
            user_manager,
            env,
            channel_id,
            "on_update_channel_slow_mode_next_send_date",
            false,
        );
    }

    /// The queued `Td::send_update` fan-out; the Td actor flushes it on
    /// the same points as the UserManager queue.
    pub(crate) fn take_updates(&mut self) -> Vec<td_api::Update> {
        std::mem::take(&mut self.pending_updates)
    }

    // ---- M.5.21 groundwork: the min-channel cache accessors
    // (ChatManager.cpp:8895-8915) ----

    /// `have_min_channel` (ChatManager.cpp:8895-8897).
    pub fn have_min_channel(&self, channel_id: ChannelId) -> bool {
        self.min_channels.contains_key(&channel_id.get())
    }

    /// `get_min_channel` (ChatManager.cpp:8899-8901).
    pub fn get_min_channel(&self, channel_id: ChannelId) -> Option<&MinChannel> {
        self.min_channels.get(&channel_id.get())
    }

    /// `add_min_channel` (ChatManager.cpp:8903-8908).
    pub fn add_min_channel(&mut self, channel_id: ChannelId, min_channel: MinChannel) {
        if self.have_channel(channel_id)
            || self.have_min_channel(channel_id)
            || !channel_id.is_valid()
        {
            return;
        }
        self.min_channels.insert(channel_id.get(), min_channel);
    }

    // ---- M.5.33 groundwork: the access checks the full-query senders
    // validate with (the full InputPeer family stays with M.5.33) ----

    /// `get_channel_type` (ChatManager.cpp:2872-2877).
    fn get_channel_type(c: &Channel) -> ChannelType {
        if c.is_megagroup {
            return ChannelType::Megagroup;
        }
        ChannelType::Broadcast
    }

    /// `is_broadcast_channel` (ChatManager.cpp:2879-2881). The payment
    /// plane's giveaway validation reads it through the env (M.43).
    pub fn is_broadcast_channel(&self, channel_id: ChannelId) -> bool {
        self.get_channel(channel_id)
            .is_some_and(|c| Self::get_channel_type(c) == ChannelType::Broadcast)
    }

    /// `is_channel_public` (ChatManager.cpp:8601-8603).
    fn is_channel_public(c: &Channel) -> bool {
        c.usernames.has_first_username() || c.has_location
    }

    /// `get_input_channel` (ChatManager.cpp:2744-2769). The
    /// `inputChannelFromMessage` branch needs `get_simple_input_peer`
    /// (M.5.33) — unreachable until `channel_messages_` is fed (M.5.43).
    pub(crate) fn get_input_channel(
        &self,
        env: &UserEnv,
        channel_id: ChannelId,
    ) -> Option<telegram_api::InputChannel> {
        let Some(c) = self.channels.get(&channel_id.get()) else {
            if env.auth_is_bot && channel_id.is_valid() {
                return Some(telegram_api::InputChannel::inputChannel(
                    telegram_api::inputChannel {
                        channel_id: channel_id.get(),
                        access_hash: 0,
                    },
                ));
            }
            if let Some(messages) = self.channel_messages.get(&channel_id.get()) {
                debug_assert!(!messages.is_empty());
                // inputChannelFromMessage(get_simple_input_peer(dialog_id), ...) —
                // the M.5.33 deferral; the map stays empty until M.5.43.
                let _ = messages;
                tdutils::log_error!("Can't get input channel from message for {channel_id:?}");
            }
            return None;
        };
        Some(telegram_api::InputChannel::inputChannel(
            telegram_api::inputChannel {
                channel_id: channel_id.get(),
                access_hash: c.access_hash,
            },
        ))
    }

    /// `have_input_peer_channel` (ChatManager.cpp:2883-2907, the
    /// `const Channel *` form). The `have_dialog_access_by_invite_link`
    /// fan-out answers false until the DialogInviteLinkManager step.
    fn have_input_peer_channel(
        &self,
        env: &UserEnv,
        c: Option<&Channel>,
        channel_id: ChannelId,
        access_rights: AccessRights,
        from_linked: bool,
    ) -> bool {
        let Some(c) = c else {
            if env.auth_is_bot && channel_id.is_valid() {
                return true;
            }
            return self.channel_messages.contains_key(&channel_id.get());
        };
        if access_rights == AccessRights::Know {
            return true;
        }
        if !from_linked && c.is_monoforum {
            if env.auth_is_bot {
                return c.is_admined_monoforum;
            }
            let monoforum_channel_id = c.monoforum_channel_id;
            let monoforum_channel = self.get_channel(monoforum_channel_id);
            if let Some(monoforum_channel) = monoforum_channel {
                return self.have_input_peer_channel(
                    env,
                    Some(monoforum_channel),
                    monoforum_channel_id,
                    AccessRights::Read,
                    true,
                );
            }
            tdutils::log_info!("Have no parent {monoforum_channel_id:?}");
            return true;
        }

        if c.status.is_administrator() {
            return true;
        }
        if c.status.is_banned() {
            return false;
        }
        if c.status.is_member() {
            return true;
        }

        let is_public = Self::is_channel_public(c);
        if access_rights == AccessRights::Read {
            if is_public {
                return true;
            }
            if !from_linked && c.has_linked_channel && c.is_megagroup {
                let linked_channel_id = self
                    .linked_channel_ids
                    .get(&channel_id.get())
                    .copied()
                    .unwrap_or_default();
                let linked_channel_id = ChannelId::new(linked_channel_id);
                if linked_channel_id.is_valid() && self.have_channel(linked_channel_id) {
                    if self.have_input_peer_channel(
                        env,
                        self.get_channel(linked_channel_id),
                        linked_channel_id,
                        access_rights,
                        true,
                    ) {
                        return true;
                    }
                } else {
                    return true;
                }
            }
            // td_->dialog_invite_link_manager_->have_dialog_access_by_invite_link
            // — the DialogInviteLinkManager stub (M.2x wave 2).
            return false;
        }

        if !from_linked && c.is_megagroup && !env.auth_is_bot && c.has_linked_channel {
            let linked_channel_id = self
                .linked_channel_ids
                .get(&channel_id.get())
                .copied()
                .unwrap_or_default();
            let linked_channel_id = ChannelId::new(linked_channel_id);
            if linked_channel_id.is_valid() && (is_public || self.have_channel(linked_channel_id)) {
                return is_public
                    || self.have_input_peer_channel(
                        env,
                        self.get_channel(linked_channel_id),
                        linked_channel_id,
                        AccessRights::Read,
                        true,
                    );
            }
            return true;
        }
        false
    }

    /// `have_input_peer_channel(ChannelId, AccessRights)` (ChatManager.cpp:2803-2806).
    pub(crate) fn have_input_peer_channel_id(
        &self,
        env: &UserEnv,
        channel_id: ChannelId,
        access_rights: AccessRights,
    ) -> bool {
        let c = self.get_channel(channel_id);
        self.have_input_peer_channel(env, c, channel_id, access_rights, false)
    }

    /// `have_input_peer_chat(const Chat *, AccessRights)`
    /// (ChatManager.cpp:2771-2793).
    fn have_input_peer_chat_record(c: Option<&Chat>, access_rights: AccessRights) -> bool {
        let Some(c) = c else {
            tdutils::log_debug!("Have no basic group");
            return false;
        };
        if access_rights == AccessRights::Know || access_rights == AccessRights::Read {
            return true;
        }
        if c.status.is_left() {
            tdutils::log_debug!("Have left basic group");
            return false;
        }
        if access_rights == AccessRights::Write && !c.is_active {
            tdutils::log_debug!("Have inactive basic group");
            return false;
        }
        true
    }

    /// `have_input_peer_chat(ChatId, AccessRights)` (ChatManager.cpp:2769-2771).
    pub(crate) fn have_input_peer_chat(
        &self,
        chat_id: ChatId,
        access_rights: AccessRights,
    ) -> bool {
        Self::have_input_peer_chat_record(self.get_chat(chat_id), access_rights)
    }

    /// `get_input_peer_chat` (ChatManager.cpp:2795-2801).
    pub fn get_input_peer_chat(
        &self,
        chat_id: ChatId,
        access_rights: AccessRights,
    ) -> Option<telegram_api::InputPeer> {
        let c = self.get_chat(chat_id);
        if !Self::have_input_peer_chat_record(c, access_rights) {
            return None;
        }
        Some(telegram_api::InputPeer::inputPeerChat(
            telegram_api::inputPeerChat {
                chat_id: chat_id.get(),
            },
        ))
    }

    /// `get_input_peer_channel` (ChatManager.cpp:2808-2829). The
    /// `channel_messages_` branch needs the M.5.43 message-id packing, so
    /// it errors like [`ChatManager::get_input_channel`] until then.
    pub fn get_input_peer_channel(
        &self,
        env: &UserEnv,
        channel_id: ChannelId,
        access_rights: AccessRights,
    ) -> Option<telegram_api::InputPeer> {
        let c = self.get_channel(channel_id);
        if !self.have_input_peer_channel(env, c, channel_id, access_rights, false) {
            return None;
        }
        let Some(c) = c else {
            if env.auth_is_bot && channel_id.is_valid() {
                return Some(telegram_api::InputPeer::inputPeerChannel(
                    telegram_api::inputPeerChannel {
                        channel_id: channel_id.get(),
                        access_hash: 0,
                    },
                ));
            }
            if self.channel_messages.contains_key(&channel_id.get()) {
                // inputPeerChannelFromMessage(get_simple_input_peer(dialog_id), ...) —
                // the M.5.43 deferral; the map stays empty until then.
                tdutils::log_error!("Can't get input peer channel from message for {channel_id:?}");
            }
            return None;
        };
        Some(telegram_api::InputPeer::inputPeerChannel(
            telegram_api::inputPeerChannel {
                channel_id: channel_id.get(),
                access_hash: c.access_hash,
            },
        ))
    }

    /// `get_simple_input_peer` (ChatManager.cpp:2831-2841).
    pub fn get_simple_input_peer(&self, dialog_id: DialogId) -> Option<telegram_api::InputPeer> {
        debug_assert!(dialog_id.get_type() == DialogType::Channel);
        let channel_id = dialog_id.get_channel_id();
        let c = self.get_channel(channel_id)?;
        Some(telegram_api::InputPeer::inputPeerChannel(
            telegram_api::inputPeerChannel {
                channel_id: channel_id.get(),
                access_hash: c.access_hash,
            },
        ))
    }

    // ---- M.5.15/M.5.16: the ChatFull/ChannelFull persistence plane ----

    /// `get_chat_full_database_key` (ChatManager.cpp:5011-5013).
    fn get_chat_full_database_key(chat_id: ChatId) -> Vec<u8> {
        format!("grf{}", chat_id.get()).into_bytes()
    }

    /// `get_channel_full_database_key` (ChatManager.cpp:5088-5090).
    fn get_channel_full_database_key(channel_id: ChannelId) -> Vec<u8> {
        format!("chf{}", channel_id.get()).into_bytes()
    }

    /// `get_channel_full_mut` — the local map accessor.
    fn get_channel_full_mut(&mut self, channel_id: ChannelId) -> Option<&mut ChannelFull> {
        self.channels_full.get_mut(&channel_id.get())
    }

    /// `get_chat_full` (ChatManager.cpp:8432-8438).
    fn get_chat_full(&self, chat_id: ChatId) -> Option<&ChatFull> {
        self.chats_full.get(&chat_id.get())
    }

    /// (the M.5.28+ participant intakes consume the mutable accessor)
    #[allow(dead_code)]
    fn get_chat_full_mut(&mut self, chat_id: ChatId) -> Option<&mut ChatFull> {
        self.chats_full.get_mut(&chat_id.get())
    }

    /// `add_chat_full` (ChatManager.cpp:8440-8447) — the M.5.23+
    /// on_get_chat_full intakes are its production callers.
    #[allow(dead_code)]
    fn add_chat_full(&mut self, chat_id: ChatId) -> &mut ChatFull {
        debug_assert!(chat_id.is_valid());
        self.chats_full.entry(chat_id.get()).or_default()
    }

    /// `get_channel_full_const` (ChatManager.cpp:8958-8960).
    fn get_channel_full_const(&self, channel_id: ChannelId) -> Option<&ChannelFull> {
        self.channels_full.get(&channel_id.get())
    }

    /// `add_channel_full` (ChatManager.cpp:8975-8982) — the M.5.25+
    /// on_get_channel_full intakes are its production callers.
    #[allow(dead_code)]
    fn add_channel_full(&mut self, channel_id: ChannelId) -> &mut ChannelFull {
        debug_assert!(channel_id.is_valid());
        self.channels_full.entry(channel_id.get()).or_default()
    }

    /// `save_chat_full` (ChatManager.cpp:4996-5004) collapsed onto the
    /// synchronous kv. Fulls carry no binlog mirror (the C++ saves only
    /// the `grf`/`chf` rows).
    fn save_chat_full(&mut self, env: &mut UserEnv, chat_id: ChatId) {
        if !env.global.use_chat_info_database() {
            return;
        }
        tdutils::log_info!("Trying to save to database full {chat_id:?}");
        let Some(chat_full) = self.chats_full.get(&chat_id.get()) else {
            debug_assert!(false, "Missing {chat_id:?} in save_chat_full");
            return;
        };
        let mut value = Vec::new();
        chat_full.store_value(&mut value);
        env.global
            .td_db()
            .common_kv()
            .set(&Self::get_chat_full_database_key(chat_id), &value);
    }

    /// `save_channel_full` (ChatManager.cpp:5073-5081).
    fn save_channel_full(&mut self, env: &mut UserEnv, channel_id: ChannelId) {
        if !env.global.use_chat_info_database() {
            return;
        }
        tdutils::log_info!("Trying to save to database full {channel_id:?}");
        let Some(channel_full) = self.channels_full.get(&channel_id.get()) else {
            debug_assert!(false, "Missing {channel_id:?} in save_channel_full");
            return;
        };
        let ctx = env.time_ctx();
        let mut value = Vec::new();
        channel_full.store_value(&mut value, &ctx);
        env.global
            .td_db()
            .common_kv()
            .set(&Self::get_channel_full_database_key(channel_id), &value);
    }

    /// `on_load_chat_full_from_database` (ChatManager.cpp:5020-5061) —
    /// the `Dependencies::resolve_force` fan-out collapses into
    /// synchronous user force-loads; the dialog/community dependencies
    /// are the documented M.6/M.12 stubs.
    fn on_load_chat_full_from_database(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        chat_id: ChatId,
        value: Vec<u8>,
    ) {
        if self.chats_full.contains_key(&chat_id.get()) || value.is_empty() {
            return;
        }
        let mut chat_full = match ChatFull::parse_value(&value) {
            Ok(chat_full) => chat_full,
            Err(error) => {
                // can't happen unless database is broken — clean all known
                // data about the chat and pretend there was nothing
                tdutils::log_error!("Repair broken full {chat_id:?}: {error}");
                self.chats_full.remove(&chat_id.get());
                env.global
                    .td_db()
                    .common_kv()
                    .erase(&Self::get_chat_full_database_key(chat_id));
                return;
            }
        };
        // dependencies: creator + participants' senders + inviters + the
        // invite-link creator. `add_message_sender_dependencies` on a
        // chat/channel/secret dialog is the DialogManager force-create stub.
        let creator_user_id = chat_full.creator_user_id;
        if creator_user_id.is_valid()
            && !user_manager.have_user_force(
                env,
                creator_user_id,
                "on_load_chat_full_from_database",
            )
        {
            self.chats_full.remove(&chat_id.get());
            env.global
                .td_db()
                .common_kv()
                .erase(&Self::get_chat_full_database_key(chat_id));
            return;
        }
        for participant in &chat_full.participants {
            if participant.dialog_id_.get_type() == DialogType::User {
                let user_id = participant.dialog_id_.get_user_id();
                if user_id.is_valid()
                    && !user_manager.have_user_force(
                        env,
                        user_id,
                        "on_load_chat_full_from_database",
                    )
                {
                    self.chats_full.remove(&chat_id.get());
                    env.global
                        .td_db()
                        .common_kv()
                        .erase(&Self::get_chat_full_database_key(chat_id));
                    return;
                }
            }
            let inviter_user_id = participant.inviter_user_id_;
            if inviter_user_id.is_valid()
                && !user_manager.have_user_force(
                    env,
                    inviter_user_id,
                    "on_load_chat_full_from_database",
                )
            {
                self.chats_full.remove(&chat_id.get());
                env.global
                    .td_db()
                    .common_kv()
                    .erase(&Self::get_chat_full_database_key(chat_id));
                return;
            }
        }
        let invite_creator_user_id = chat_full.invite_link.get_creator_user_id();
        if invite_creator_user_id.is_valid()
            && !user_manager.have_user_force(
                env,
                invite_creator_user_id,
                "on_load_chat_full_from_database",
            )
        {
            self.chats_full.remove(&chat_id.get());
            env.global
                .td_db()
                .common_kv()
                .erase(&Self::get_chat_full_database_key(chat_id));
            return;
        }

        // `CHECK(get_chat(chat_id) != nullptr)` — the caller force-loaded it.
        let (need_invite_link, c_is_active, c_photo_small_valid) = {
            let Some(c) = self.get_chat(chat_id) else {
                debug_assert!(
                    false,
                    "Missing chat {chat_id:?} in on_load_chat_full_from_database"
                );
                return;
            };
            (
                c.is_active && c.status.can_manage_invite_links(),
                c.is_active,
                c.photo.small_file_id.is_valid(),
            )
        };
        let _ = c_is_active;
        let have_invite_link = chat_full.invite_link.is_valid();
        if need_invite_link != have_invite_link {
            if need_invite_link {
                // ignore ChatFull without invite link
                self.chats_full.remove(&chat_id.get());
                return;
            }
            chat_full.invite_link = DialogInviteLink::default();
        }

        let c_photo_same = self
            .get_chat(chat_id)
            .is_some_and(|c| is_same_dialog_photo(&chat_full.photo, &c.photo));
        // auto photo = std::move(chat_full->photo); chat_full->photo = Photo();
        let mut photo = std::mem::take(&mut chat_full.photo);
        if !c_photo_same {
            photo = Photo::default();
            if c_photo_small_valid {
                // reload_chat_full(chat_id, Auto(), ...) — an empty promise
                self.send_get_chat_full_query(
                    env,
                    chat_id,
                    Promise::default(),
                    "on_load_chat_full_from_database",
                );
            }
        }

        // td_->group_call_manager_->on_update_dialog_about — the
        // GroupCallManager stub (wave 3).
        self.chats_full.insert(chat_id.get(), chat_full);
        self.on_update_chat_full_photo(env, chat_id, photo);
        let f = self.chats_full.get_mut(&chat_id.get()).unwrap();
        f.is_update_chat_full_sent = true;
        self.update_chat_full(
            user_manager,
            env,
            chat_id,
            "on_load_chat_full_from_database",
            true,
        );
    }

    /// `get_chat_full_force` (ChatManager.cpp:5063-5081) — the synchronous
    /// db-load path behind the `unavailable_chat_fulls_` one-shot gate.
    fn get_chat_full_force(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        chat_id: ChatId,
        source: &str,
    ) -> Option<&mut ChatFull> {
        if !self.have_chat_force(user_manager, env, chat_id, source) {
            return None;
        }
        if self.chats_full.contains_key(&chat_id.get()) {
            return self.chats_full.get_mut(&chat_id.get());
        }
        if !env.global.use_chat_info_database() {
            return None;
        }
        if !self.unavailable_chat_fulls.insert(chat_id.get()) {
            return None;
        }
        tdutils::log_info!("Trying to load full {chat_id:?} from database from {source}");
        let value = env
            .global
            .td_db()
            .common_kv()
            .get(&Self::get_chat_full_database_key(chat_id));
        self.on_load_chat_full_from_database(user_manager, env, chat_id, value);
        self.chats_full.get_mut(&chat_id.get())
    }

    /// `is_chat_full_outdated` (ChatManager.cpp:8449-8477) over the
    /// snapshot of the `Chat` fields it reads (the load path cannot hold
    /// a `&Chat` across the force-load; the snapshot carries them).
    fn is_chat_full_outdated(
        &self,
        full_version: i32,
        has_valid_invite_link: bool,
        full_photo: &Photo,
        c: &ChatSnapshot,
        chat_id: ChatId,
        only_participants: bool,
    ) -> bool {
        if !c.is_active && full_version == -1 {
            return false;
        }
        if full_version != c.version {
            tdutils::log_info!(
                "Have outdated ChatFull {chat_id:?} with current version {full_version} and chat version {}",
                c.version
            );
            return true;
        }
        if !only_participants && c.is_active && c.can_manage_invite_links && !has_valid_invite_link
        {
            tdutils::log_info!("Have outdated invite link in {chat_id:?}");
            return true;
        }
        if !only_participants && !is_same_dialog_photo(full_photo, &c.photo) {
            tdutils::log_info!("Have outdated chat photo in {chat_id:?}");
            return true;
        }
        false
    }

    /// `load_chat_full` (ChatManager.cpp:8479-8510). The story-manager
    /// view fan-out is the StoryManager stub.
    pub fn load_chat_full(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        chat_id: ChatId,
        force: bool,
        mut promise: Promise<()>,
        source: &str,
    ) {
        let Some(c_snapshot) = self.get_chat(chat_id).map(|c| ChatSnapshot {
            is_active: c.is_active,
            version: c.version,
            can_manage_invite_links: c.status.can_manage_invite_links(),
            photo: c.photo.clone(),
        }) else {
            return promise.set_error(Status::error(400, "Group not found"));
        };
        let full_snapshot = self
            .get_chat_full_force(user_manager, env, chat_id, source)
            .map(|f| (f.version, f.invite_link.is_valid(), f.photo.clone()));
        let is_outdated = match full_snapshot {
            Some((version, has_valid_invite_link, photo)) => self.is_chat_full_outdated(
                version,
                has_valid_invite_link,
                &photo,
                &c_snapshot,
                chat_id,
                false,
            ),
            None => {
                tdutils::log_info!("Full {chat_id:?} not found");
                return self.send_get_chat_full_query(env, chat_id, promise, source);
            }
        };
        if is_outdated {
            tdutils::log_info!("Have outdated full {chat_id:?}");
            if env.auth_is_bot && !force {
                return self.send_get_chat_full_query(env, chat_id, promise, source);
            }
            self.send_get_chat_full_query(env, chat_id, Promise::default(), source);
        }
        // td_->story_manager_->on_view_dialog_active_stories(participants)
        // — the StoryManager stub (wave 3).
        promise.set_value(());
    }

    /// `send_get_chat_full_query` (ChatManager.cpp:8513-8525) — the
    /// QueryCombiner + GetFullChatQuery send is `[blocked: net]`; the
    /// validation surfaces stay exact.
    fn send_get_chat_full_query(
        &mut self,
        env: &mut UserEnv,
        chat_id: ChatId,
        mut promise: Promise<()>,
        source: &str,
    ) {
        tdutils::log_info!("Get full {chat_id:?} from {source}");
        if !chat_id.is_valid() {
            return promise.set_error(Status::error(500, "Invalid chat_id"));
        }
        let _ = env;
        tdutils::log_info!("GetFullChatQuery for {chat_id:?} — [blocked: net]");
        promise.set_error(Status::error(500, "Network is not implemented yet"));
    }

    /// `reload_chat_full` (ChatManager.cpp:8512).
    pub fn reload_chat_full(
        &mut self,
        env: &mut UserEnv,
        chat_id: ChatId,
        promise: Promise<()>,
        source: &str,
    ) {
        self.send_get_chat_full_query(env, chat_id, promise, source);
    }

    /// `reload_chat` (ChatManager.cpp:8421-8429) — the GetChatsQuery send
    /// is [blocked: net] (M.5.44); the validation surfaces stay exact.
    fn reload_chat(
        &mut self,
        env: &mut UserEnv,
        chat_id: ChatId,
        mut promise: Promise<()>,
        source: &str,
    ) {
        tdutils::log_info!("Reload {chat_id:?} from {source}");
        if env.global.close_flag() {
            return promise.set_error(Status::error(400, "The server is temporarily unavailable"));
        }
        if !chat_id.is_valid() {
            return promise.set_error(Status::error(400, "Invalid basic group identifier"));
        }
        let _ = source;
        tdutils::log_info!("GetChatsQuery for {chat_id:?} — [blocked: net]");
        promise.set_error(Status::error(500, "Network is not implemented yet"));
    }

    /// `reload_channel` (ChatManager.cpp:8938-8946) — the GetChannels
    /// query send is [blocked: net] (M.5.44); the validation surfaces
    /// stay exact.
    fn reload_channel(
        &mut self,
        env: &mut UserEnv,
        channel_id: ChannelId,
        mut promise: Promise<()>,
        source: &str,
    ) {
        tdutils::log_info!("Reload {channel_id:?} from {source}");
        if env.global.close_flag() {
            return promise.set_error(Status::error(400, "The server is temporarily unavailable"));
        }
        if !channel_id.is_valid() {
            return promise.set_error(Status::error(400, "Invalid supergroup identifier"));
        }
        let _ = source;
        tdutils::log_info!("GetChannelsQuery for {channel_id:?} — [blocked: net]");
        promise.set_error(Status::error(500, "Network is not implemented yet"));
    }

    /// `on_load_channel_full_from_database` (ChatManager.cpp:5098-5220).
    /// The dialog/community dependencies are the documented M.6/M.12
    /// stubs; the user dependencies collapse into synchronous
    /// force-loads. `update_channel` is still the M.5.19 slot.
    fn on_load_channel_full_from_database(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        channel_id: ChannelId,
        value: Vec<u8>,
        source: &str,
        is_recursive: bool,
    ) {
        if self.channels_full.contains_key(&channel_id.get()) || value.is_empty() {
            return;
        }
        let parsed = {
            let ctx = env.time_ctx();
            ChannelFull::parse_value(&value, &ctx)
        };
        let mut channel_full = match parsed {
            Ok(channel_full) => channel_full,
            Err(error) => {
                // can't happen unless database is broken
                tdutils::log_error!("Repair broken full {channel_id:?}: {error}");
                self.channels_full.remove(&channel_id.get());
                env.global
                    .td_db()
                    .common_kv()
                    .erase(&Self::get_channel_full_database_key(channel_id));
                return;
            }
        };
        // must not depend on the linked/monoforum dialogs themselves (the
        // message database can be disabled) — the Dialogs are forcibly
        // created in update_channel_full (the DialogManager stub);
        // add_dialog_dependencies + the community dependency land with
        // their managers.
        let mut user_dependencies: Vec<UserId> = Vec::new();
        user_dependencies.push(channel_full.guard_bot_user_id);
        user_dependencies.extend(channel_full.bot_user_ids.iter().copied());
        if let Some(bot_verification) = &channel_full.bot_verification {
            user_dependencies.push(bot_verification.bot_user_id_);
        }
        user_dependencies.push(channel_full.invite_link.get_creator_user_id());
        for user_id in user_dependencies {
            if user_id.is_valid() && !user_manager.have_user_force(env, user_id, source) {
                self.channels_full.remove(&channel_id.get());
                env.global
                    .td_db()
                    .common_kv()
                    .erase(&Self::get_channel_full_database_key(channel_id));
                return;
            }
        }
        let migrated_from_chat_id = channel_full.migrated_from_chat_id;
        if migrated_from_chat_id.is_valid()
            && !self.have_chat_force(user_manager, env, migrated_from_chat_id, source)
        {
            self.channels_full.remove(&channel_id.get());
            env.global
                .td_db()
                .common_kv()
                .erase(&Self::get_channel_full_database_key(channel_id));
            return;
        }

        // `CHECK(get_channel(channel_id) != nullptr)` — the caller
        // force-loaded it.
        let need_invite_link = self
            .get_channel(channel_id)
            .is_some_and(|c| c.status.can_manage_invite_links());
        let have_invite_link = channel_full.invite_link.is_valid();
        if need_invite_link != have_invite_link {
            if need_invite_link {
                // ignore ChannelFull without invite link
                self.channels_full.remove(&channel_id.get());
                return;
            }
            channel_full.invite_link = DialogInviteLink::default();
        }

        let c_photo_small_valid = self
            .get_channel(channel_id)
            .is_some_and(|c| c.photo.small_file_id.is_valid());
        let c_photo_same = self
            .get_channel(channel_id)
            .is_some_and(|c| is_same_dialog_photo(&channel_full.photo, &c.photo));
        let mut photo = std::mem::take(&mut channel_full.photo);
        if !c_photo_same {
            photo = Photo::default();
            if c_photo_small_valid {
                channel_full.expires_at = 0.0;
            }
        }

        if channel_full.participant_count < channel_full.administrator_count {
            channel_full.participant_count = channel_full.administrator_count;
        }
        let c_participant_count = self.get_channel(channel_id).map(|c| c.participant_count);
        if let Some(c_participant_count) = c_participant_count
            && c_participant_count != 0
            && c_participant_count != channel_full.participant_count
        {
            channel_full.participant_count = c_participant_count;
            if channel_full.participant_count < channel_full.administrator_count {
                channel_full.participant_count = channel_full.administrator_count;
                channel_full.expires_at = 0.0;
                let c = self.get_channel_mut(channel_id).unwrap();
                c.participant_count = channel_full.participant_count;
                c.is_changed = true;
            }
        }
        let c_can_be_deleted = self.get_channel(channel_id).map(|c| c.can_be_deleted);
        if let Some(c_can_be_deleted) = c_can_be_deleted
            && c_can_be_deleted != channel_full.can_be_deleted
        {
            let c = self.get_channel_mut(channel_id).unwrap();
            c.can_be_deleted = channel_full.can_be_deleted;
            c.need_save_to_database = true;
        }

        let c_is_slow_mode_enabled = self
            .get_channel(channel_id)
            .is_some_and(|c| c.is_slow_mode_enabled);
        let was_invalidated = self.invalidated_channels_full.remove(&channel_id.get());
        let drop_slow_mode = !c_is_slow_mode_enabled;
        if was_invalidated || (!c_is_slow_mode_enabled && channel_full.slow_mode_delay != 0) {
            Self::do_invalidate_channel_full_record(&mut channel_full, drop_slow_mode);
        }

        // td_->group_call_manager_->on_update_dialog_about — the
        // GroupCallManager stub (wave 3).
        self.channels_full.insert(channel_id.get(), channel_full);
        self.on_update_channel_full_photo(env, channel_id, photo);

        // send_closure_later(messages_manager on_dialog_bots_updated) —
        // the MessagesManager stub.
        self.update_channel(user_manager, env, channel_id, false, false);

        let f = self.channels_full.get_mut(&channel_id.get()).unwrap();
        f.is_update_channel_full_sent = true;
        self.update_channel_full(user_manager, env, channel_id, source, true);

        let expires_at = self
            .channels_full
            .get(&channel_id.get())
            .map(|f| f.expires_at)
            .unwrap_or(0.0);
        if expires_at == 0.0 {
            self.load_channel_full(
                user_manager,
                env,
                channel_id,
                true,
                Promise::default(),
                "on_load_channel_full_from_database",
            );
        }

        let monoforum_channel_id = self
            .channels_full
            .get(&channel_id.get())
            .map(|f| f.monoforum_channel_id)
            .unwrap_or_default();
        if monoforum_channel_id.is_valid()
            && self.get_channel_full_const(monoforum_channel_id).is_none()
            && (is_recursive
                || self
                    .get_channel_full_force(
                        user_manager,
                        env,
                        monoforum_channel_id,
                        true,
                        source,
                        true,
                    )
                    .is_none())
        {
            {
                tdutils::log_info!(
                    "Can't find full {monoforum_channel_id:?} from full {channel_id:?}"
                );
                self.reload_channel_full(env, monoforum_channel_id, Promise::default(), source);
            }
        }
    }

    /// `get_channel_full_force` (ChatManager.cpp:5222-5241).
    fn get_channel_full_force(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        channel_id: ChannelId,
        only_local: bool,
        source: &str,
        is_recursive: bool,
    ) -> Option<&mut ChannelFull> {
        if !self.have_channel_force(user_manager, env, channel_id, source) {
            return None;
        }
        if self.channels_full.contains_key(&channel_id.get()) {
            return self.channels_full.get_mut(&channel_id.get());
        }
        if !env.global.use_chat_info_database() {
            return None;
        }
        if !self.unavailable_channel_fulls.insert(channel_id.get()) {
            return None;
        }
        tdutils::log_info!("Trying to load full {channel_id:?} from database from {source}");
        let value = env
            .global
            .td_db()
            .common_kv()
            .get(&Self::get_channel_full_database_key(channel_id));
        self.on_load_channel_full_from_database(
            user_manager,
            env,
            channel_id,
            value,
            source,
            is_recursive,
        );
        if only_local || self.channels_full.contains_key(&channel_id.get()) {
            return self.channels_full.get_mut(&channel_id.get());
        }
        // C++ returns get_channel_full(channel_id, only_local, source) —
        // which re-checks the expiry and may send the query.
        self.get_channel_full(env, channel_id, only_local, source)
    }

    /// `get_channel_full` (ChatManager.cpp:8962-8973).
    fn get_channel_full(
        &mut self,
        env: &mut UserEnv,
        channel_id: ChannelId,
        only_local: bool,
        source: &str,
    ) -> Option<&mut ChannelFull> {
        let is_expired = self
            .channels_full
            .get(&channel_id.get())
            .map(|f| f.is_expired());
        if is_expired == Some(true) && !only_local && !env.auth_is_bot {
            self.send_get_channel_full_query(env, channel_id, Promise::default(), source);
        }
        self.channels_full.get_mut(&channel_id.get())
    }

    /// `load_channel_full` (ChatManager.cpp:8984-9013).
    pub fn load_channel_full(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        channel_id: ChannelId,
        force: bool,
        mut promise: Promise<()>,
        source: &str,
    ) {
        let is_expired =
            match self.get_channel_full_force(user_manager, env, channel_id, true, source, false) {
                Some(channel_full) => channel_full.is_expired(),
                None => {
                    // the C++ passes the null ChannelFull — the version
                    // bookkeeping inside skips
                    return self.send_get_channel_full_query(env, channel_id, promise, source);
                }
            };
        if is_expired {
            if env.auth_is_bot && !force {
                return self.send_get_channel_full_query(
                    env,
                    channel_id,
                    promise,
                    "load expired channel_full",
                );
            }
            // the reload result only logs in C++
            self.send_get_channel_full_query(
                env,
                channel_id,
                Promise::default(),
                "load expired channel_full",
            );
        }
        promise.set_value(());
    }

    /// `send_get_channel_full_query` (ChatManager.cpp:9015-9048) — the
    /// GetFullChannelQuery send is `[blocked: net]`; the validation and
    /// the `repair_request_version` bookkeeping stay exact.
    fn send_get_channel_full_query(
        &mut self,
        env: &mut UserEnv,
        channel_id: ChannelId,
        mut promise: Promise<()>,
        source: &str,
    ) {
        let input_channel = self.get_input_channel(env, channel_id);
        if input_channel.is_none() {
            return promise.set_error(Status::error(400, "Supergroup not found"));
        }
        if !self.have_input_peer_channel_id(env, channel_id, AccessRights::Read) {
            return promise.set_error(Status::error(400, "Can't access the chat"));
        }
        let bookkeeping = self
            .channels_full
            .get(&channel_id.get())
            .map(|f| (f.repair_request_version, f.speculative_version));
        if let Some((repair_request_version, speculative_version)) = bookkeeping {
            if promise.is_empty() {
                if repair_request_version != 0 {
                    tdutils::log_info!("Skip get full {channel_id:?} request from {source}");
                    return;
                }
                let f = self.channels_full.get_mut(&channel_id.get()).unwrap();
                f.repair_request_version = speculative_version;
            } else {
                let f = self.channels_full.get_mut(&channel_id.get()).unwrap();
                f.repair_request_version = u32::MAX;
            }
        }
        tdutils::log_info!("Get full {channel_id:?} from {source}");
        let _ = input_channel;
        tdutils::log_info!("GetFullChannelQuery for {channel_id:?} — [blocked: net]");
        promise.set_error(Status::error(500, "Network is not implemented yet"));
    }

    /// `reload_channel_full` (ChatManager.cpp:9028-9031).
    pub fn reload_channel_full(
        &mut self,
        env: &mut UserEnv,
        channel_id: ChannelId,
        promise: Promise<()>,
        source: &str,
    ) {
        self.send_get_channel_full_query(env, channel_id, promise, source);
    }

    // ---- M.5.27 (pulled forward): the invalidation machine the unban
    // timeout and the load path ride on ----

    /// `invalidate_channel_full` (ChatManager.cpp:6638-6647).
    pub(crate) fn invalidate_channel_full(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        channel_id: ChannelId,
        need_drop_slow_mode_delay: bool,
        source: &str,
    ) {
        tdutils::log_info!("Invalidate supergroup full for {channel_id:?} from {source}");
        if self.channels_full.contains_key(&channel_id.get()) {
            // must not load ChannelFull
            self.do_invalidate_channel_full(channel_id, need_drop_slow_mode_delay);
            self.update_channel_full(user_manager, env, channel_id, source, false);
        } else if channel_id.is_valid() {
            self.invalidated_channels_full.insert(channel_id.get());
        }
    }

    /// `do_invalidate_channel_full` (ChatManager.cpp:6649-6663) over the
    /// map record — the post-insert form. The dialog_manager
    /// full-invalidation fan-out is the DialogManager stub.
    fn do_invalidate_channel_full(
        &mut self,
        channel_id: ChannelId,
        need_drop_slow_mode_delay: bool,
    ) {
        let Some(f) = self.channels_full.get_mut(&channel_id.get()) else {
            debug_assert!(
                false,
                "Missing full {channel_id:?} in do_invalidate_channel_full"
            );
            return;
        };
        Self::do_invalidate_channel_full_record(f, need_drop_slow_mode_delay);
    }

    /// `do_invalidate_channel_full(ChannelFull *, ...)` — the record form
    /// the load path runs before the parsed full is inserted.
    fn do_invalidate_channel_full_record(f: &mut ChannelFull, need_drop_slow_mode_delay: bool) {
        // td_->dialog_manager_->on_dialog_info_full_invalidated — the
        // DialogManager stub.
        if f.expires_at >= tdutils::time::now() {
            f.expires_at = 0.0;
            f.need_save_to_database = true;
        }
        if need_drop_slow_mode_delay && f.slow_mode_delay != 0 {
            f.slow_mode_delay = 0;
            f.slow_mode_next_send_date = 0;
            f.is_slow_mode_next_send_date_changed = true;
            f.is_changed = true;
        }
    }

    /// `drop_chat_full` (ChatManager.cpp:7576-7594). The
    /// `update_chat_online_member_count` fan-out is the
    /// GroupCallManager/MessagesManager stub; the M.5.29 intake is the
    /// production caller.
    #[allow(dead_code)]
    pub(crate) fn drop_chat_full(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        chat_id: ChatId,
    ) {
        if self
            .get_chat_full_force(user_manager, env, chat_id, "drop_chat_full")
            .is_none()
        {
            return;
        }
        tdutils::log_info!("Drop basicGroupFullInfo of {chat_id:?}");
        self.on_update_chat_full_photo(env, chat_id, Photo::default());
        {
            let Some(f) = self.chats_full.get_mut(&chat_id.get()) else {
                return;
            };
            // chat_full->creator_user_id = UserId();
            f.participants.clear();
            f.bot_commands.clear();
            f.version = -1;
            Self::update_permanent_invite_link(&mut f.invite_link, DialogInviteLink::default());
            f.is_changed = true;
        }
        self.update_chat_full(user_manager, env, chat_id, "drop_chat_full", false);
    }

    // ---- M.5.17: the full-info file-source edges (the Files-step
    // stand-ins) ----

    /// The `FileReferenceManager::create_*_file_source` stand-in — a local
    /// counter (replaced by the real registry with the Files slice).
    fn create_file_source_id(&mut self) -> FileSourceId {
        self.file_source_id_counter += 1;
        FileSourceId::new(self.file_source_id_counter)
    }

    /// `get_chat_full_file_source_id` (ChatManager.cpp:8320-8337).
    pub fn get_chat_full_file_source_id(
        &mut self,
        env: &mut UserEnv,
        chat_id: ChatId,
    ) -> FileSourceId {
        if !chat_id.is_valid() {
            return FileSourceId::default();
        }
        if let Some(chat_full) = self.get_chat_full(chat_id) {
            // chat full was already added, source ID was registered and
            // shouldn't be needed
            return if chat_full.is_update_chat_full_sent {
                FileSourceId::default()
            } else {
                chat_full.file_source_id
            };
        }
        let _ = env;
        match self.chat_full_file_source_ids.get(&chat_id.get()) {
            Some(source_id) if source_id.is_valid() => *source_id,
            _ => {
                let new_source_id = self.create_file_source_id();
                self.chat_full_file_source_ids
                    .insert(chat_id.get(), new_source_id);
                new_source_id
            }
        }
    }

    /// `get_channel_full_file_source_id` (ChatManager.cpp:8339-8356).
    pub fn get_channel_full_file_source_id(
        &mut self,
        env: &mut UserEnv,
        channel_id: ChannelId,
    ) -> FileSourceId {
        if !channel_id.is_valid() {
            return FileSourceId::default();
        }
        if let Some(channel_full) = self.get_channel_full_const(channel_id) {
            // channel full was already added, source ID was registered and
            // shouldn't be needed
            return if channel_full.is_update_channel_full_sent {
                FileSourceId::default()
            } else {
                channel_full.file_source_id
            };
        }
        let _ = env;
        match self.channel_full_file_source_ids.get(&channel_id.get()) {
            Some(source_id) if source_id.is_valid() => *source_id,
            _ => {
                let new_source_id = self.create_file_source_id();
                self.channel_full_file_source_ids
                    .insert(channel_id.get(), new_source_id);
                new_source_id
            }
        }
    }

    // ---- the full-photo appliers (ChatManager.cpp:6664-6722). The
    // `change_files_source` fan-out is the Files-step no-op; the
    // registered-file-ids bookkeeping stays. ----

    /// `on_update_chat_full_photo` (ChatManager.cpp:6664-6690).
    fn on_update_chat_full_photo(&mut self, env: &mut UserEnv, chat_id: ChatId, photo: Photo) {
        let _ = env;
        if !self.chats_full.contains_key(&chat_id.get()) {
            debug_assert!(
                false,
                "Missing full {chat_id:?} in on_update_chat_full_photo"
            );
            return;
        }
        let photo_file_ids = {
            let Some(f) = self.chats_full.get_mut(&chat_id.get()) else {
                return;
            };
            if photo != f.photo {
                f.photo = photo;
                f.is_changed = true;
            }
            photo_get_file_ids(&f.photo)
        };
        let same = self
            .chats_full
            .get(&chat_id.get())
            .is_some_and(|f| f.registered_photo_file_ids == photo_file_ids);
        if same {
            return;
        }
        let need_source = !self
            .chats_full
            .get(&chat_id.get())
            .is_some_and(|f| f.file_source_id.is_valid());
        if need_source {
            let moved_source = match self
                .chat_full_file_source_ids
                .get(&chat_id.get())
                .copied()
                .filter(|id| id.is_valid())
            {
                // move the file source inside of the full
                Some(stored) => {
                    self.chat_full_file_source_ids.remove(&chat_id.get());
                    stored
                }
                // need to create a new file source for the full
                None => self.create_file_source_id(),
            };
            if let Some(f) = self.chats_full.get_mut(&chat_id.get()) {
                f.file_source_id = moved_source;
            }
        }
        if let Some(f) = self.chats_full.get_mut(&chat_id.get()) {
            f.registered_photo_file_ids = photo_file_ids;
        }
    }

    /// `on_update_channel_full_photo` (ChatManager.cpp:6692-6722).
    fn on_update_channel_full_photo(
        &mut self,
        env: &mut UserEnv,
        channel_id: ChannelId,
        photo: Photo,
    ) {
        let _ = env;
        if !self.channels_full.contains_key(&channel_id.get()) {
            debug_assert!(
                false,
                "Missing full {channel_id:?} in on_update_channel_full_photo"
            );
            return;
        }
        let photo_file_ids = {
            let Some(f) = self.channels_full.get_mut(&channel_id.get()) else {
                return;
            };
            if photo != f.photo {
                f.photo = photo;
                f.is_changed = true;
                f.is_photo_changed = true;
            }
            photo_get_file_ids(&f.photo)
        };
        let same = self
            .channels_full
            .get(&channel_id.get())
            .is_some_and(|f| f.registered_photo_file_ids == photo_file_ids);
        if same {
            return;
        }
        let need_source = !self
            .channels_full
            .get(&channel_id.get())
            .is_some_and(|f| f.file_source_id.is_valid());
        if need_source {
            let moved_source = match self
                .channel_full_file_source_ids
                .get(&channel_id.get())
                .copied()
                .filter(|id| id.is_valid())
            {
                Some(stored) => {
                    self.channel_full_file_source_ids.remove(&channel_id.get());
                    stored
                }
                None => self.create_file_source_id(),
            };
            if let Some(f) = self.channels_full.get_mut(&channel_id.get()) {
                f.file_source_id = moved_source;
            }
        }
        if let Some(f) = self.channels_full.get_mut(&channel_id.get()) {
            f.registered_photo_file_ids = photo_file_ids;
        }
    }

    // ---- the permanent-invite-link machinery (ChatManager.cpp:6724-6743,
    // :6982-6995) ----

    /// `update_permanent_invite_link` (ChatManager.cpp:6982-6995). The
    /// `dialog_invite_link_manager_->invalidate_invite_link_info`
    /// fan-out is the DialogInviteLinkManager stub (wave 2).
    #[allow(dead_code)]
    fn update_permanent_invite_link(
        invite_link: &mut DialogInviteLink,
        new_invite_link: DialogInviteLink,
    ) -> bool {
        if new_invite_link != *invite_link {
            // if (invite_link.is_valid() && invite_link.get_invite_link() != ...)
            //   — invalidate the old link's info (stub)
            *invite_link = new_invite_link;
            return true;
        }
        false
    }

    /// `on_update_chat_full_invite_link` (ChatManager.cpp:6724-6731) —
    /// the M.5.24 intake caller lands with on_get_chat_full.
    #[allow(dead_code)]
    fn on_update_chat_full_invite_link(
        &mut self,
        env: &mut UserEnv,
        chat_id: ChatId,
        invite_link: Option<telegram_api::ExportedChatInvite>,
    ) {
        let _ = env;
        let Some(f) = self.chats_full.get_mut(&chat_id.get()) else {
            debug_assert!(
                false,
                "Missing full {chat_id:?} in on_update_chat_full_invite_link"
            );
            return;
        };
        let new_invite_link =
            DialogInviteLink::from_telegram(invite_link, false, false, "ChatFull");
        if Self::update_permanent_invite_link(&mut f.invite_link, new_invite_link) {
            f.is_changed = true;
        }
    }

    /// `on_update_channel_full_invite_link` (ChatManager.cpp:6733-6740) —
    /// the M.5.26 intake caller lands with on_get_channel_full.
    #[allow(dead_code)]
    fn on_update_channel_full_invite_link(
        &mut self,
        env: &mut UserEnv,
        channel_id: ChannelId,
        invite_link: Option<telegram_api::ExportedChatInvite>,
    ) {
        let _ = env;
        let Some(f) = self.channels_full.get_mut(&channel_id.get()) else {
            debug_assert!(
                false,
                "Missing full {channel_id:?} in on_update_channel_full_invite_link"
            );
            return;
        };
        let new_invite_link =
            DialogInviteLink::from_telegram(invite_link, false, false, "ChannelFull");
        if Self::update_permanent_invite_link(&mut f.invite_link, new_invite_link) {
            f.is_changed = true;
        }
    }

    // ---- M.5.24/M.5.26 (cores pulled forward): the full diff machines
    // (ChatManager.cpp:5558-5620, :5622-5700) ----

    /// `update_chat_full` (ChatManager.cpp:5558-5620). The
    /// dialog_participant_manager/messages_manager fan-outs are the
    /// documented stubs.
    fn update_chat_full(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        chat_id: ChatId,
        source: &str,
        from_database: bool,
    ) {
        if self
            .chats_full
            .get(&chat_id.get())
            .is_some_and(|f| f.is_being_updated)
        {
            tdutils::log_error!("Detected recursive update of full {chat_id:?} from {source}");
        }
        if let Some(f) = self.chats_full.get_mut(&chat_id.get()) {
            f.is_being_updated = true;
        }
        self.unavailable_chat_fulls.remove(&chat_id.get());
        let (need_send_update, need_save) = {
            let Some(f) = self.chats_full.get_mut(&chat_id.get()) else {
                debug_assert!(false, "Missing full {chat_id:?} in update_chat_full");
                return;
            };
            f.need_send_update |= f.is_changed;
            f.need_save_to_database |= f.is_changed;
            f.is_changed = false;
            (f.need_send_update, f.need_save_to_database)
        };
        if need_send_update || need_save {
            tdutils::log_info!("Update full {chat_id:?} from {source}");
        }
        if need_send_update {
            // the administrators + bots lists (the stub fan-outs consume
            // them in C++)
            let mut bot_user_ids: Vec<UserId> = Vec::new();
            let mut administrators: Vec<crate::chat_manager::DialogAdministrator> = Vec::new();
            let participants = self
                .chats_full
                .get(&chat_id.get())
                .map(|f| f.participants.clone())
                .unwrap_or_default();
            for participant in &participants {
                if participant.dialog_id_.get_type() == DialogType::User {
                    let user_id = participant.dialog_id_.get_user_id();
                    if participant.status_.is_administrator() {
                        administrators.push(DialogAdministrator::from_status(
                            user_id,
                            &participant.status_,
                        ));
                    }
                    if user_manager.is_user_bot_of(user_id) {
                        bot_user_ids.push(user_id);
                    }
                }
            }
            if let Some(f) = self.chats_full.get_mut(&chat_id.get()) {
                f.bot_commands
                    .retain(|commands| bot_user_ids.contains(&commands.get_bot_user_id()));
            }
            // td_->dialog_participant_manager_->on_update_dialog_administrators
            // and messages_manager on_dialog_bots_updated — the stubs.
            let _ = administrators;

            {
                let c_is_sent = self
                    .get_chat(chat_id)
                    .map(|c| c.is_update_basic_group_sent)
                    .unwrap_or(false);
                // CHECK(c == nullptr || c->is_update_basic_group_sent)
                debug_assert!(self.get_chat(chat_id).is_none() || c_is_sent);
            }
            let basic_group_id = self.get_basic_group_id_object(chat_id, "update_chat_full");
            let full_object = self.get_basic_group_full_info_object(user_manager, env, chat_id);
            if let Some(f) = self.chats_full.get_mut(&chat_id.get()) {
                if !f.is_update_chat_full_sent {
                    tdutils::log_error!(
                        "Send partial updateBasicGroupFullInfo for {chat_id:?} from {source}"
                    );
                    f.is_update_chat_full_sent = true;
                }
                f.need_send_update = false;
            }
            self.pending_updates
                .push(td_api::Update::updateBasicGroupFullInfo(
                    td_api::updateBasicGroupFullInfo {
                        basic_group_id,
                        basic_group_full_info: Some(full_object),
                    },
                ));
        }
        if need_save {
            if !from_database {
                self.save_chat_full(env, chat_id);
            }
            if let Some(f) = self.chats_full.get_mut(&chat_id.get()) {
                f.need_save_to_database = false;
            }
        }
        if let Some(f) = self.chats_full.get_mut(&chat_id.get()) {
            f.is_being_updated = false;
        }
    }

    /// `update_channel_full` (ChatManager.cpp:5622-5700). The
    /// dialog_manager force-create and the monoforum photo special-case
    /// keep their C++ shape; the linked/monoforum dialog creation is the
    /// DialogManager stub.
    fn update_channel_full(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        channel_id: ChannelId,
        source: &str,
        from_database: bool,
    ) {
        if self
            .channels_full
            .get(&channel_id.get())
            .is_some_and(|f| f.is_being_updated)
        {
            tdutils::log_error!("Detected recursive update of full {channel_id:?} from {source}");
        }
        if let Some(f) = self.channels_full.get_mut(&channel_id.get()) {
            f.is_being_updated = true;
        }
        self.unavailable_channel_fulls.remove(&channel_id.get());
        {
            // CHECK(channel_full->participant_count >= administrator_count)
            let counts = self
                .channels_full
                .get(&channel_id.get())
                .map(|f| (f.participant_count, f.administrator_count));
            debug_assert!(counts.is_none_or(|(p, a)| p >= a));
        }
        if self
            .channels_full
            .get(&channel_id.get())
            .is_some_and(|f| f.is_slow_mode_next_send_date_changed)
        {
            let now = env.global.server_time();
            let f = self.channels_full.get_mut(&channel_id.get()).unwrap();
            if f.slow_mode_next_send_date as f64 > now + 3601.0 {
                f.slow_mode_next_send_date = (now as i32) + 3601;
            }
            if (f.slow_mode_next_send_date as f64) <= now {
                f.slow_mode_next_send_date = 0;
            }
            if f.slow_mode_next_send_date == 0 {
                Self::cancel_timeout(&mut self.slow_mode_delay_timeouts, channel_id);
            } else {
                let timeout_in = f.slow_mode_next_send_date as f64 - now + 0.002;
                Self::set_timeout(&mut self.slow_mode_delay_timeouts, channel_id, timeout_in);
            }
            f.is_slow_mode_next_send_date_changed = false;
        }
        if self
            .channels_full
            .get(&channel_id.get())
            .is_some_and(|f| f.is_photo_changed)
        {
            let emit_monoforum = {
                let Some(f) = self.channels_full.get(&channel_id.get()) else {
                    return;
                };
                f.monoforum_channel_id.is_valid() && self.is_broadcast_channel(channel_id)
            };
            if emit_monoforum {
                let monoforum_channel_id = self
                    .channels_full
                    .get(&channel_id.get())
                    .map(|f| f.monoforum_channel_id)
                    .unwrap_or_default();
                let monoforum_sent = self
                    .channels_full
                    .get(&monoforum_channel_id.get())
                    .is_some_and(|f| f.is_update_channel_full_sent);
                if monoforum_sent {
                    let update = self.get_update_supergroup_full_info_object(
                        user_manager,
                        env,
                        monoforum_channel_id,
                        "update_channel_full monoforum",
                    );
                    self.pending_updates.push(update);
                }
            }
            let f = self.channels_full.get_mut(&channel_id.get()).unwrap();
            f.is_photo_changed = false;
        }
        let (need_send_update, need_save) = {
            let Some(f) = self.channels_full.get_mut(&channel_id.get()) else {
                return;
            };
            if f.need_save_to_database {
                let bot_user_ids = f.bot_user_ids.clone();
                let before = f.bot_commands.len();
                f.bot_commands
                    .retain(|commands| bot_user_ids.contains(&commands.get_bot_user_id()));
                f.is_changed |= before != f.bot_commands.len();
            }
            f.need_send_update |= f.is_changed;
            f.need_save_to_database |= f.is_changed;
            f.is_changed = false;
            (f.need_send_update, f.need_save_to_database)
        };
        if need_send_update || need_save {
            tdutils::log_info!("Update full {channel_id:?} from {source}");
        }
        if need_send_update {
            let (linked_valid, linked_id, monoforum_valid, monoforum_id) = self
                .channels_full
                .get(&channel_id.get())
                .map(|f| {
                    (
                        f.linked_channel_id.is_valid(),
                        f.linked_channel_id,
                        f.monoforum_channel_id.is_valid(),
                        f.monoforum_channel_id,
                    )
                })
                .unwrap_or((false, ChannelId::default(), false, ChannelId::default()));
            // dialog_manager_->force_create_dialog(linked/monoforum) — the
            // DialogManager stub (M.6).
            let _ = (linked_valid, linked_id, monoforum_valid, monoforum_id);

            let update =
                self.get_update_supergroup_full_info_object(user_manager, env, channel_id, source);
            if let Some(f) = self.channels_full.get_mut(&channel_id.get()) {
                if !f.is_update_channel_full_sent {
                    tdutils::log_error!(
                        "Send partial updateSupergroupFullInfo for {channel_id:?} from {source}"
                    );
                    f.is_update_channel_full_sent = true;
                }
                f.need_send_update = false;
            }
            self.pending_updates.push(update);
        }
        if need_save {
            if !from_database {
                self.save_channel_full(env, channel_id);
            }
            if let Some(f) = self.channels_full.get_mut(&channel_id.get()) {
                f.need_save_to_database = false;
            }
        }
        if let Some(f) = self.channels_full.get_mut(&channel_id.get()) {
            f.is_being_updated = false;
        }
    }

    // ---- the query-failure handlers (ChatManager.cpp:6235-6251) ----

    /// `on_get_chat_full_failed` (ChatManager.cpp:6235-6241).
    pub fn on_get_chat_full_failed(&mut self, env: &mut UserEnv, chat_id: ChatId) {
        if env.global.close_flag() {
            return;
        }
        tdutils::log_info!("Failed to get full {chat_id:?}");
    }

    /// `on_get_channel_full_failed` (ChatManager.cpp:6243-6251).
    pub fn on_get_channel_full_failed(
        &mut self,
        _user_manager: &mut UserManager,
        env: &mut UserEnv,
        channel_id: ChannelId,
    ) {
        if env.global.close_flag() {
            return;
        }
        tdutils::log_info!("Failed to get full {channel_id:?}");
        if let Some(channel_full) =
            self.get_channel_full(env, channel_id, true, "on_get_channel_full")
        {
            channel_full.repair_request_version = 0;
        }
    }

    // ---- M.5.36/M.5.37 (cores pulled forward): the id + object builders
    // the full diff machines emit through (ChatManager.cpp:9783-9962) ----

    /// `get_basic_group_id_object` (ChatManager.cpp:9795-9803).
    pub fn get_basic_group_id_object(&mut self, chat_id: ChatId, source: &str) -> i64 {
        if chat_id.is_valid()
            && self.get_chat(chat_id).is_none()
            && !self.unknown_chats.contains(&chat_id.get())
        {
            tdutils::log_error!("Have no information about {chat_id:?} from {source}");
            self.unknown_chats.insert(chat_id.get());
            let update = Self::get_update_unknown_basic_group_object(chat_id);
            self.pending_updates.push(update);
        }
        chat_id.get()
    }

    /// `get_update_unknown_basic_group_object` (ChatManager.cpp:9790-9793).
    pub fn get_update_unknown_basic_group_object(chat_id: ChatId) -> td_api::Update {
        td_api::Update::updateBasicGroup(td_api::updateBasicGroup {
            basic_group: Some(td_api::BasicGroup {
                id: chat_id.get(),
                member_count: 0,
                status: Some(
                    DialogParticipantStatus::banned(0, String::new())
                        .get_chat_member_status_object(),
                ),
                is_active: true,
                upgraded_to_supergroup_id: 0,
            }),
        })
    }

    /// `get_supergroup_id_object` (ChatManager.cpp:9862-9877).
    pub fn get_supergroup_id_object(&mut self, channel_id: ChannelId, source: &str) -> i64 {
        if channel_id.is_valid()
            && self.get_channel(channel_id).is_none()
            && !self.unknown_channels.contains(&channel_id.get())
        {
            if self.have_min_channel(channel_id) {
                tdutils::log_info!("Have only min {channel_id:?} received from {source}");
            } else {
                tdutils::log_error!(
                    "Have no information about {channel_id:?} received from {source}"
                );
            }
            self.unknown_channels.insert(channel_id.get());
            let update = self.get_update_unknown_supergroup_object(channel_id);
            self.pending_updates.push(update);
        }
        channel_id.get()
    }

    /// `get_update_unknown_supergroup_object` (ChatManager.cpp:9886-9897).
    /// The nullable object/enum fields answer the codegen-forced empty
    /// payloads.
    pub fn get_update_unknown_supergroup_object(&self, channel_id: ChannelId) -> td_api::Update {
        let is_megagroup = self
            .get_min_channel(channel_id)
            .is_some_and(|m| m.is_megagroup_);
        td_api::Update::updateSupergroup(td_api::updateSupergroup {
            supergroup: Some(td_api::Supergroup {
                id: channel_id.get(),
                usernames: Some(td_api::Usernames {
                    active_usernames: Vec::new(),
                    disabled_usernames: Vec::new(),
                    editable_username: String::new(),
                    collectible_usernames: Vec::new(),
                    secondary_username: String::new(),
                }),
                date: 0,
                status: Some(
                    DialogParticipantStatus::banned(0, String::new())
                        .get_chat_member_status_object(),
                ),
                member_count: 0,
                boost_level: 0,
                has_automatic_translation: false,
                has_linked_chat: false,
                has_location: false,
                sign_messages: false,
                show_message_sender: false,
                join_to_send_messages: !is_megagroup,
                join_by_request: false,
                is_slow_mode_enabled: false,
                is_channel: !is_megagroup,
                is_broadcast_group: false,
                is_forum: false,
                is_direct_messages_group: false,
                is_administered_direct_messages_group: false,
                verification_status: Some(td_api::VerificationStatus::default()),
                has_direct_messages_group: false,
                has_forum_tabs: false,
                restriction_info: Some(td_api::RestrictionInfo::default()),
                paid_message_star_count: 0,
                active_story_state: Some(td_api::ActiveStoryState::default()),
            }),
        })
    }

    /// `get_basic_group_object(chat_id, c)` (ChatManager.cpp:9817-9824).
    pub fn get_basic_group_object_const(
        &mut self,
        chat_id: ChatId,
        c: &Chat,
    ) -> td_api::BasicGroup {
        let migrated = c.migrated_to_channel_id;
        td_api::BasicGroup {
            id: chat_id.get(),
            member_count: c.participant_count,
            status: Some(c.status.get_chat_member_status_object()),
            is_active: c.is_active,
            upgraded_to_supergroup_id: self
                .get_supergroup_id_object(migrated, "get_basic_group_object"),
        }
    }

    /// `get_basic_group_object(chat_id)` (ChatManager.cpp:9812-9814).
    pub fn get_basic_group_object(&mut self, chat_id: ChatId) -> Option<td_api::BasicGroup> {
        let c = self.get_chat(chat_id).cloned()?;
        Some(self.get_basic_group_object_const(chat_id, &c))
    }

    /// `get_update_basic_group_object` (ChatManager.cpp:9782-9788).
    pub fn get_update_basic_group_object(
        &mut self,
        chat_id: ChatId,
        c: Option<&Chat>,
    ) -> td_api::Update {
        match c {
            None => Self::get_update_unknown_basic_group_object(chat_id),
            Some(c) => {
                let basic_group = self.get_basic_group_object_const(chat_id, c);
                td_api::Update::updateBasicGroup(td_api::updateBasicGroup {
                    basic_group: Some(basic_group),
                })
            }
        }
    }

    /// `get_channel_join_to_send` (ChatManager.cpp static).
    fn get_channel_join_to_send(c: &Channel) -> bool {
        (c.join_to_send || !c.is_megagroup || !c.has_linked_channel) && !c.is_monoforum
    }

    /// `get_channel_join_request` (ChatManager.cpp static).
    fn get_channel_join_request(c: &Channel) -> bool {
        c.join_request && c.is_megagroup && !c.is_monoforum && !c.is_gigagroup
    }

    /// `get_channel_verification_status_object` (ChatManager.cpp).
    fn get_channel_verification_status_object(c: &Channel) -> Option<td_api::VerificationStatus> {
        get_verification_status_object(c.is_verified, c.is_scam, c.is_fake, c.bot_verification_icon)
    }

    /// `get_channel_active_story_state` +
    /// `ActiveStoryState::get_active_story_state_object` — the story-state
    /// triple of the channel.
    fn get_channel_active_story_state_object(c: &Channel) -> td_api::ActiveStoryState {
        if c.has_live_story {
            td_api::ActiveStoryState::activeStoryStateLive(td_api::activeStoryStateLive {
                story_id: 0,
            })
        } else if c.max_read_story_id.get() < c.max_active_story_id.get() {
            td_api::ActiveStoryState::activeStoryStateUnread(td_api::activeStoryStateUnread {})
        } else {
            td_api::ActiveStoryState::activeStoryStateRead(td_api::activeStoryStateRead {})
        }
    }

    /// `get_supergroup_object(channel_id, c)` (ChatManager.cpp:9909-9931).
    pub fn get_supergroup_object_const(
        &mut self,
        channel_id: ChannelId,
        c: &Channel,
    ) -> td_api::Supergroup {
        let restriction_reasons = c.restriction_reasons.clone();
        td_api::Supergroup {
            id: channel_id.get(),
            usernames: Some(c.usernames.get_usernames_object()),
            date: c.date,
            status: Some(c.status.get_chat_member_status_object()),
            member_count: c.participant_count,
            boost_level: c.boost_level,
            has_automatic_translation: c.autotranslation,
            has_linked_chat: c.has_linked_channel,
            has_location: c.has_location,
            sign_messages: c.sign_messages,
            show_message_sender: c.show_message_sender,
            join_to_send_messages: Self::get_channel_join_to_send(c),
            join_by_request: Self::get_channel_join_request(c),
            is_slow_mode_enabled: c.is_slow_mode_enabled,
            is_channel: !c.is_megagroup,
            is_broadcast_group: c.is_gigagroup,
            is_forum: c.is_forum,
            is_direct_messages_group: c.is_monoforum,
            is_administered_direct_messages_group: c.is_admined_monoforum,
            verification_status: Some(
                Self::get_channel_verification_status_object(c).unwrap_or_default(),
            ),
            has_direct_messages_group: c.broadcast_messages_allowed,
            has_forum_tabs: c.is_forum_tabs,
            restriction_info: Some(
                get_restriction_info_object(
                    &restriction_reasons,
                    &RestrictionOptions {
                        ignored_restriction_reasons: String::new(),
                        restriction_add_platforms: String::new(),
                        ignore_platform_restrictions: false,
                    },
                )
                .unwrap_or_default(),
            ),
            paid_message_star_count: c.paid_message_star_count,
            active_story_state: Some(Self::get_channel_active_story_state_object(c)),
        }
    }

    /// `get_supergroup_object(channel_id)` (ChatManager.cpp:9904-9907).
    pub fn get_supergroup_object(&mut self, channel_id: ChannelId) -> Option<td_api::Supergroup> {
        let c = self.get_channel(channel_id).cloned()?;
        Some(self.get_supergroup_object_const(channel_id, &c))
    }

    /// `get_update_supergroup_object` (ChatManager.cpp:9879-9884).
    pub fn get_update_supergroup_object(
        &mut self,
        channel_id: ChannelId,
        c: Option<&Channel>,
    ) -> td_api::Update {
        match c {
            None => self.get_update_unknown_supergroup_object(channel_id),
            Some(c) => {
                let supergroup = self.get_supergroup_object_const(channel_id, c);
                td_api::Update::updateSupergroup(td_api::updateSupergroup {
                    supergroup: Some(supergroup),
                })
            }
        }
    }

    /// `get_chat_member_object` (ChatManager.cpp:6383-6391). The
    /// `messageSenderChat` branch carries the unified td_api chat object
    /// (the C++ routes through `dialog_manager_->get_chat_id_object`).
    pub(crate) fn get_chat_member_object(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        dialog_participant: &DialogParticipant,
        source: &str,
    ) -> td_api::ChatMember {
        let rank = dialog_participant.status_.get_rank().to_owned();
        let member_id = self.get_message_sender_object(
            user_manager,
            env,
            dialog_participant.dialog_id_,
            source,
        );
        let inviter_user_id = user_manager.get_user_id_object(
            env,
            dialog_participant.inviter_user_id_,
            "chatMember.inviter_user_id",
        );
        td_api::ChatMember {
            member_id: Some(member_id),
            tag: rank,
            inviter_user_id,
            joined_chat_date: dialog_participant.joined_date_,
            status: Some(dialog_participant.status_.get_chat_member_status_object()),
        }
    }

    /// `get_message_sender_object` (MessageSender.cpp:60-66). The C++
    /// gates the chat branch on `messages_manager_->have_dialog` (M.6/M.10)
    /// and force-creates the dialog; the stand-in answers
    /// `messageSenderChat(dialog_id)` directly, which is the steady-state
    /// answer once those land.
    pub(crate) fn get_message_sender_object(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        dialog_id: DialogId,
        source: &str,
    ) -> td_api::MessageSender {
        let _ = self;
        match dialog_id.get_type() {
            DialogType::User => {
                let user_id = user_manager.get_user_id_object(env, dialog_id.get_user_id(), source);
                td_api::MessageSender::messageSenderUser(td_api::messageSenderUser { user_id })
            }
            DialogType::Chat | DialogType::Channel | DialogType::SecretChat => {
                // dialog_manager_->get_chat_id_object — the raw dialog id
                td_api::MessageSender::messageSenderChat(td_api::messageSenderChat {
                    chat_id: dialog_id.get(),
                })
            }
            DialogType::None => {
                tdutils::log_error!(
                    "Receive invalid {dialog_id:?} in get_message_sender_object from {source}"
                );
                td_api::MessageSender::messageSenderUser(td_api::messageSenderUser { user_id: 0 })
            }
        }
    }

    /// `get_basic_group_full_info_object(chat_id, chat_full)`
    /// (ChatManager.cpp:9827-9848).
    pub fn get_basic_group_full_info_object(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        chat_id: ChatId,
    ) -> td_api::BasicGroupFullInfo {
        let Some(chat_full) = self.chats_full.get(&chat_id.get()) else {
            debug_assert!(
                false,
                "Missing full {chat_id:?} in get_basic_group_full_info_object"
            );
            return td_api::BasicGroupFullInfo {
                photo: Some(get_chat_photo_object(&Photo::default())),
                description: String::new(),
                creator_user_id: 0,
                members: Vec::new(),
                can_hide_members: false,
                can_toggle_aggressive_anti_spam: false,
                invite_link: Some(td_api::ChatInviteLink::default()),
                bot_commands: Vec::new(),
            };
        };
        let photo = chat_full.photo.clone();
        let description = chat_full.description.clone();
        let creator_user_id = chat_full.creator_user_id;
        let participants = chat_full.participants.clone();
        let invite_link = chat_full.invite_link.clone();
        let bot_commands = chat_full.bot_commands.clone();

        let creator_user_id =
            user_manager.get_user_id_object(env, creator_user_id, "basicGroupFullInfo");
        let members = participants
            .iter()
            .map(|participant| {
                self.get_chat_member_object(
                    user_manager,
                    env,
                    participant,
                    "get_basic_group_full_info_object",
                )
            })
            .collect();
        let can_hide_members = self.can_hide_chat_participants(env, chat_id).is_ok();
        let can_toggle_aggressive_anti_spam = self
            .can_toggle_chat_aggressive_anti_spam(env, chat_id)
            .is_ok();
        let invite_link_object = invite_link
            .get_chat_invite_link_object(user_manager, env)
            .unwrap_or_default();
        let bot_commands = bot_commands
            .iter()
            .map(|commands| commands.get_bot_commands_object(user_manager, env))
            .collect();
        td_api::BasicGroupFullInfo {
            photo: Some(get_chat_photo_object(&photo)),
            description,
            creator_user_id,
            members,
            can_hide_members,
            can_toggle_aggressive_anti_spam,
            invite_link: Some(invite_link_object),
            bot_commands,
        }
    }

    /// `get_supergroup_full_info_object(channel_id, channel_full, c)`
    /// (ChatManager.cpp:9927-9953). The `channel_full` is cloned at the
    /// call boundary (the C++ holds the pointer); the community-id object
    /// is the CommunityManager stand-in (valid → raw id, else 0).
    pub fn get_supergroup_full_info_object(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        channel_id: ChannelId,
        channel_full: &ChannelFull,
        c: Option<&Channel>,
    ) -> td_api::SupergroupFullInfo {
        let mut slow_mode_delay_expires_in = 0.0f64;
        if channel_full.slow_mode_next_send_date != 0
            && (channel_full.unrestrict_boost_count == 0
                || channel_full.boost_count < channel_full.unrestrict_boost_count)
        {
            slow_mode_delay_expires_in =
                (channel_full.slow_mode_next_send_date as f64 - env.global.server_time()).max(1e-3);
        }
        let has_hidden_participants =
            channel_full.has_hidden_participants || !channel_full.can_get_participants;
        let mut photo = channel_full.photo.clone();
        if c.is_some_and(|c| c.is_monoforum)
            && let Some(monoforum_channel_full) =
                self.get_channel_full_const(channel_full.monoforum_channel_id)
        {
            photo = monoforum_channel_full.photo.clone();
        }
        let bot_commands = channel_full
            .bot_commands
            .iter()
            .map(|commands| commands.get_bot_commands_object(user_manager, env))
            .collect();
        let bot_verification = channel_full
            .bot_verification
            .as_ref()
            .map(|v| v.get_bot_verification_object(user_manager, env));
        let invite_link = channel_full
            .invite_link
            .clone()
            .get_chat_invite_link_object(user_manager, env);
        let guard_bot_user_id = user_manager.get_user_id_object(
            env,
            channel_full.guard_bot_user_id,
            "supergroupFullInfo guard bot",
        );
        let can_hide_members = self
            .can_hide_channel_participants(env, channel_id, Some(channel_full))
            .is_ok();
        let can_toggle_aggressive_anti_spam = self
            .can_toggle_channel_aggressive_anti_spam(env, channel_id, Some(channel_full))
            .is_ok();
        let migrated_from_basic_group_id = {
            let migrated_from_chat_id = channel_full.migrated_from_chat_id;
            self.get_basic_group_id_object(migrated_from_chat_id, "get_supergroup_full_info_object")
        };
        let community_id = if channel_full.linked_community_id.is_valid() {
            channel_full.linked_community_id.get()
        } else {
            0
        };
        td_api::SupergroupFullInfo {
            photo: Some(get_chat_photo_object(&photo)),
            community_id,
            description: channel_full.description.clone(),
            member_count: channel_full.participant_count,
            administrator_count: channel_full.administrator_count,
            restricted_count: channel_full.restricted_count,
            banned_count: channel_full.banned_count,
            linked_chat_id: DialogId::from_channel_id(channel_full.linked_channel_id).get(),
            direct_messages_chat_id: DialogId::from_channel_id(channel_full.monoforum_channel_id)
                .get(),
            slow_mode_delay: channel_full.slow_mode_delay,
            slow_mode_delay_expires_in,
            can_enable_paid_messages: channel_full.has_paid_messages_available,
            can_enable_paid_reaction: channel_full.has_paid_media_allowed,
            can_get_members: channel_full.can_get_participants,
            has_hidden_members: has_hidden_participants,
            can_hide_members,
            can_set_sticker_set: channel_full.can_set_sticker_set,
            can_set_location: channel_full.can_set_location,
            can_get_statistics: channel_full.can_view_statistics,
            can_get_revenue_statistics: channel_full.can_view_revenue,
            can_get_star_revenue_statistics: channel_full.can_view_star_revenue,
            can_send_gift: channel_full.has_stargifts_available,
            can_toggle_aggressive_anti_spam,
            is_all_history_available: channel_full.is_all_history_available,
            can_have_sponsored_messages: channel_full.can_have_sponsored_messages,
            has_aggressive_anti_spam_enabled: channel_full.has_aggressive_anti_spam_enabled,
            has_paid_media_allowed: channel_full.has_paid_media_allowed,
            has_pinned_stories: channel_full.has_pinned_stories,
            gift_count: channel_full.gift_count,
            my_boost_count: channel_full.boost_count,
            unrestrict_boost_count: channel_full.unrestrict_boost_count,
            outgoing_paid_message_star_count: channel_full.send_paid_message_stars,
            sticker_set_id: channel_full.sticker_set_id.get(),
            custom_emoji_sticker_set_id: channel_full.emoji_sticker_set_id.get(),
            location: channel_full.location.get_chat_location_object(),
            invite_link,
            guard_bot_user_id,
            bot_commands,
            bot_verification,
            main_profile_tab: Some(get_profile_tab_object(channel_full.main_profile_tab)),
            upgraded_from_basic_group_id: migrated_from_basic_group_id,
            upgraded_from_max_message_id: channel_full.migrated_from_max_message_id.get(),
        }
    }

    /// `get_update_supergroup_full_info_object`
    /// (ChatManager.cpp:9955-9961). The full/channel are cloned at the
    /// lookup boundary.
    pub fn get_update_supergroup_full_info_object(
        &mut self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
        channel_id: ChannelId,
        source: &str,
    ) -> td_api::Update {
        let c = self.get_channel(channel_id).cloned();
        debug_assert!(c.as_ref().is_none_or(|c| c.is_update_supergroup_sent));
        let channel_full = self.get_channel_full_const(channel_id).cloned();
        let Some(channel_full) = channel_full else {
            debug_assert!(
                false,
                "Missing full {channel_id:?} in get_update_supergroup_full_info_object"
            );
            return self.get_update_unknown_supergroup_object(channel_id);
        };
        let supergroup_id = self.get_supergroup_id_object(channel_id, source);
        let supergroup_full_info = self.get_supergroup_full_info_object(
            user_manager,
            env,
            channel_id,
            &channel_full,
            c.as_ref(),
        );
        td_api::Update::updateSupergroupFullInfo(td_api::updateSupergroupFullInfo {
            supergroup_id,
            supergroup_full_info: Some(supergroup_full_info),
        })
    }

    // ---- the can-hide/can-toggle Status checks
    // (ChatManager.cpp:3581-3638) ----

    /// `can_hide_chat_participants` (ChatManager.cpp:3581-3596).
    pub(crate) fn can_hide_chat_participants(
        &self,
        env: &UserEnv,
        chat_id: ChatId,
    ) -> tdutils::status::Result<()> {
        let Some(c) = self.get_chat(chat_id) else {
            return tdutils::status::Result::Err(Status::error(400, "Basic group not found"));
        };
        if !c.status.is_creator() {
            return tdutils::status::Result::Err(Status::error(
                400,
                "Not enough rights to hide group members",
            ));
        }
        if (c.participant_count as i64)
            < env
                .options
                .get_option_integer("hidden_members_group_size_min", 0)
        {
            return tdutils::status::Result::Err(Status::error(
                400,
                "The basic group is too small",
            ));
        }
        tdutils::status::Result::Ok(())
    }

    /// `can_hide_channel_participants` (ChatManager.cpp:3598-3614).
    pub(crate) fn can_hide_channel_participants(
        &self,
        env: &UserEnv,
        channel_id: ChannelId,
        channel_full: Option<&ChannelFull>,
    ) -> tdutils::status::Result<()> {
        let Some(c) = self.get_channel(channel_id) else {
            return tdutils::status::Result::Err(Status::error(400, "Supergroup not found"));
        };
        if !c.status.can_restrict_members() {
            return tdutils::status::Result::Err(Status::error(
                400,
                "Not enough rights to hide group members",
            ));
        }
        if Self::get_channel_type(c) != ChannelType::Megagroup {
            return tdutils::status::Result::Err(Status::error(
                400,
                "Group members are hidden by default in channels",
            ));
        }
        if channel_full.is_some_and(|f| f.has_hidden_participants) {
            return tdutils::status::Result::Ok(());
        }
        if c.participant_count > 0
            && (c.participant_count as i64)
                < env
                    .options
                    .get_option_integer("hidden_members_group_size_min", 0)
        {
            return tdutils::status::Result::Err(Status::error(400, "The supergroup is too small"));
        }
        tdutils::status::Result::Ok(())
    }

    /// `can_toggle_chat_aggressive_anti_spam` (ChatManager.cpp:3620-3632).
    pub(crate) fn can_toggle_chat_aggressive_anti_spam(
        &self,
        env: &UserEnv,
        chat_id: ChatId,
    ) -> tdutils::status::Result<()> {
        let Some(c) = self.get_chat(chat_id) else {
            return tdutils::status::Result::Err(Status::error(400, "Basic group not found"));
        };
        if !c.status.is_creator() {
            return tdutils::status::Result::Err(Status::error(
                400,
                "Not enough rights to enable aggressive anti-spam checks",
            ));
        }
        if (c.participant_count as i64)
            < env
                .options
                .get_option_integer("aggressive_anti_spam_supergroup_member_count_min", 0)
        {
            return tdutils::status::Result::Err(Status::error(
                400,
                "The basic group is too small",
            ));
        }
        tdutils::status::Result::Ok(())
    }

    /// `can_toggle_channel_aggressive_anti_spam`
    /// (ChatManager.cpp:3634-3647+).
    pub(crate) fn can_toggle_channel_aggressive_anti_spam(
        &self,
        env: &UserEnv,
        channel_id: ChannelId,
        channel_full: Option<&ChannelFull>,
    ) -> tdutils::status::Result<()> {
        let Some(c) = self.get_channel(channel_id) else {
            return tdutils::status::Result::Err(Status::error(400, "Supergroup not found"));
        };
        if !c.status.can_delete_messages() {
            return tdutils::status::Result::Err(Status::error(
                400,
                "Not enough rights to enable aggressive anti-spam checks",
            ));
        }
        if Self::get_channel_type(c) != ChannelType::Megagroup {
            return tdutils::status::Result::Err(Status::error(
                400,
                "Aggressive anti-spam checks can be enabled in supergroups only",
            ));
        }
        if c.is_gigagroup {
            return tdutils::status::Result::Err(Status::error(
                400,
                "Aggressive anti-spam checks can't be enabled in broadcast supergroups",
            ));
        }
        if channel_full.is_some_and(|f| f.has_aggressive_anti_spam_enabled) {
            return tdutils::status::Result::Ok(());
        }
        if c.has_location
            || c.usernames
                .get_editable_username()
                .starts_with("translation_")
        {
            return tdutils::status::Result::Ok(());
        }
        if c.participant_count > 0
            && (c.participant_count as i64)
                < env
                    .options
                    .get_option_integer("aggressive_anti_spam_supergroup_member_count_min", 0)
        {
            return tdutils::status::Result::Err(Status::error(400, "The supergroup is too small"));
        }
        tdutils::status::Result::Ok(())
    }
}

impl Default for ChatManager {
    fn default() -> Self {
        ChatManager::new()
    }
}

/// The snapshot of the `Chat` fields `is_chat_full_outdated` reads — the
/// load path cannot hold a `&Chat` across the force-load.
struct ChatSnapshot {
    is_active: bool,
    version: i32,
    can_manage_invite_links: bool,
    photo: DialogPhoto,
}

/// `is_same_dialog_photo` (DialogPhoto.cpp:202-210) — the
/// `get_unique_file_id` indirection collapses to raw `FileId` equality
/// without the FileManager (the Files step).
fn is_same_dialog_photo(photo: &Photo, dialog_photo: &DialogPhoto) -> bool {
    let fake_photo = DialogPhoto::as_fake_dialog_photo(photo, DialogId::default(), false);
    fake_photo.small_file_id == dialog_photo.small_file_id
        && fake_photo.big_file_id == dialog_photo.big_file_id
}

/// `get_profile_tab_object` (ProfileTab.cpp:122-150) — `ProfileTab::Default`
/// answers the codegen-forced empty payload where the C++ returns null.
/// `is_allowed_profile_tab` (ProfileTab.cpp:12-26).
fn is_allowed_profile_tab(profile_tab: ProfileTab, channel_type: ChannelType) -> bool {
    match channel_type {
        ChannelType::Broadcast => true,
        ChannelType::Megagroup => profile_tab != ProfileTab::Gifts,
        ChannelType::Unknown => {
            profile_tab == ProfileTab::Posts || profile_tab == ProfileTab::Gifts
        }
    }
}

/// `get_profile_tab` (ProfileTab.cpp:27-53).
/// `get_profile_tab` (ProfileTab.cpp:30-53) — takes the nullable wire
/// object (the generated `Option` mirrors the C++ `object_ptr`); `None`
/// is the C++ `nullptr → ProfileTab::Default` branch.
fn get_profile_tab(
    profile_tab: Option<telegram_api::ProfileTab>,
    channel_type: ChannelType,
) -> ProfileTab {
    let Some(profile_tab) = profile_tab else {
        return ProfileTab::Default;
    };
    let result = match profile_tab {
        telegram_api::ProfileTab::profileTabPosts(_) => ProfileTab::Posts,
        telegram_api::ProfileTab::profileTabGifts(_) => ProfileTab::Gifts,
        telegram_api::ProfileTab::profileTabMedia(_) => ProfileTab::Media,
        telegram_api::ProfileTab::profileTabFiles(_) => ProfileTab::Files,
        telegram_api::ProfileTab::profileTabMusic(_) => ProfileTab::Music,
        telegram_api::ProfileTab::profileTabVoice(_) => ProfileTab::Voice,
        telegram_api::ProfileTab::profileTabLinks(_) => ProfileTab::Links,
        telegram_api::ProfileTab::profileTabGifs(_) => ProfileTab::Gifs,
    };
    if !is_allowed_profile_tab(result, channel_type) {
        tdutils::log_error!("Receive {result:?} for {channel_type:?}");
        ProfileTab::Default
    } else {
        result
    }
}

/// `get_profile_tab_tdapi` (ProfileTab.cpp:61-90) — the td_api → internal
/// conversion with the allow-matrix gate. `None` input answers `None`
/// (the caller errors "Profile tab must be non-empty" — kept at the call
/// site to match the C++ promise flow).
fn get_profile_tab_tdapi(
    profile_tab: Option<&td_api::ProfileTab>,
    channel_type: ChannelType,
) -> Option<ProfileTab> {
    let profile_tab = profile_tab?;
    let result = match profile_tab {
        td_api::ProfileTab::profileTabPosts(_) => ProfileTab::Posts,
        td_api::ProfileTab::profileTabGifts(_) => ProfileTab::Gifts,
        td_api::ProfileTab::profileTabMedia(_) => ProfileTab::Media,
        td_api::ProfileTab::profileTabFiles(_) => ProfileTab::Files,
        td_api::ProfileTab::profileTabMusic(_) => ProfileTab::Music,
        td_api::ProfileTab::profileTabVoice(_) => ProfileTab::Voice,
        td_api::ProfileTab::profileTabLinks(_) => ProfileTab::Links,
        td_api::ProfileTab::profileTabGifs(_) => ProfileTab::Gifs,
    };
    if !is_allowed_profile_tab(result, channel_type) {
        tdutils::log_error!("Receive {result:?} for {channel_type:?}");
        None
    } else {
        Some(result)
    }
}

fn get_profile_tab_object(profile_tab: ProfileTab) -> td_api::ProfileTab {
    match profile_tab {
        ProfileTab::Default => td_api::ProfileTab::default(),
        ProfileTab::Posts => td_api::ProfileTab::profileTabPosts(td_api::profileTabPosts {}),
        ProfileTab::Gifts => td_api::ProfileTab::profileTabGifts(td_api::profileTabGifts {}),
        ProfileTab::Media => td_api::ProfileTab::profileTabMedia(td_api::profileTabMedia {}),
        ProfileTab::Files => td_api::ProfileTab::profileTabFiles(td_api::profileTabFiles {}),
        ProfileTab::Links => td_api::ProfileTab::profileTabLinks(td_api::profileTabLinks {}),
        ProfileTab::Music => td_api::ProfileTab::profileTabMusic(td_api::profileTabMusic {}),
        ProfileTab::Voice => td_api::ProfileTab::profileTabVoice(td_api::profileTabVoice {}),
        ProfileTab::Gifs => td_api::ProfileTab::profileTabGifs(td_api::profileTabGifs {}),
    }
}

impl DialogParticipantStatus {
    /// `get_chat_member_status_object` (DialogParticipant.cpp:497-518).
    /// The C++ `string *rank` out-parameter is read by the caller through
    /// `get_rank()` instead.
    pub(crate) fn get_chat_member_status_object(&self) -> td_api::ChatMemberStatus {
        if matches!(self.type_, StatusType::Creator) {
            return td_api::ChatMemberStatus::chatMemberStatusCreator(
                td_api::chatMemberStatusCreator {
                    is_anonymous: self.is_anonymous(),
                    is_member: self.is_member(),
                },
            );
        }
        if matches!(self.type_, StatusType::Administrator) {
            return td_api::ChatMemberStatus::chatMemberStatusAdministrator(
                td_api::chatMemberStatusAdministrator {
                    can_be_edited: self.can_be_edited(),
                    rights: Some(
                        self.get_administrator_rights()
                            .get_chat_administrator_rights_object(),
                    ),
                },
            );
        }
        if matches!(self.type_, StatusType::Member) {
            return td_api::ChatMemberStatus::chatMemberStatusMember(
                td_api::chatMemberStatusMember {
                    member_until_date: self.until_date_,
                },
            );
        }
        if matches!(self.type_, StatusType::Restricted) {
            return td_api::ChatMemberStatus::chatMemberStatusRestricted(
                td_api::chatMemberStatusRestricted {
                    is_member: self.is_member(),
                    restricted_until_date: self.until_date_,
                    permissions: Some(self.get_restricted_rights().get_chat_permissions_object()),
                },
            );
        }
        if matches!(self.type_, StatusType::Left) {
            return td_api::ChatMemberStatus::chatMemberStatusLeft(td_api::chatMemberStatusLeft {});
        }
        td_api::ChatMemberStatus::chatMemberStatusBanned(td_api::chatMemberStatusBanned {
            banned_until_date: self.until_date_,
        })
    }
}

impl BotCommand {
    /// `get_bot_command_object` — the td_api surface of the record.
    pub(crate) fn get_bot_command_object(&self) -> td_api::BotCommand {
        td_api::BotCommand {
            command: self.command_.clone(),
            description: self.description_.clone(),
            is_ephemeral: self.is_ephemeral_,
        }
    }
}

impl BotCommands {
    /// `BotCommands::get_bot_commands_object` (BotCommand.cpp:138-142).
    pub(crate) fn get_bot_commands_object(
        &self,
        user_manager: &mut UserManager,
        env: &mut UserEnv,
    ) -> td_api::BotCommands {
        td_api::BotCommands {
            bot_user_id: user_manager.get_user_id_object(
                env,
                self.bot_user_id_,
                "get_bot_commands_object",
            ),
            commands: self
                .commands_
                .iter()
                .map(|c| c.get_bot_command_object())
                .collect(),
        }
    }
}

impl DialogLocation {
    /// `DialogLocation::get_chat_location_object` — `None` where the C++
    /// returns a null object.
    pub(crate) fn get_chat_location_object(&self) -> Option<td_api::ChatLocation> {
        if self.empty() {
            return None;
        }
        Some(td_api::ChatLocation {
            location: Some(td_api::Location {
                latitude: self.location_.latitude_,
                longitude: self.location_.longitude_,
                horizontal_accuracy: self.location_.horizontal_accuracy_,
            }),
            address: self.address_.clone(),
        })
    }
}

impl ChatFull {
    /// `log_event_store(ChatFull)` — the version-prefixed database value.
    pub fn store_value(&self, out: &mut Vec<u8>) {
        crate::store_parse::store_log_event_version(out);
        self.store(out);
    }
}

impl ChannelFull {
    /// `log_event_store(ChannelFull)` — the version-prefixed database value.
    pub fn store_value(&self, out: &mut Vec<u8>, ctx: &TimeCtx) {
        crate::store_parse::store_log_event_version(out);
        self.store(out, ctx);
    }

    /// `is_expired` (ChatManager.h:665-667).
    pub fn is_expired(&self) -> bool {
        self.expires_at < tdutils::time::now()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialog_id::{ScheduledServerMessageId, ServerMessageId};

    #[test]
    fn chat_id_bounds_match_the_cpp() {
        assert_eq!(ChatId::MAX_CHAT_ID, 999_999_999_999);
        assert!(!ChatId::new(0).is_valid());
        assert!(ChatId::new(1).is_valid());
        assert!(ChatId::new(ChatId::MAX_CHAT_ID).is_valid());
        assert!(!ChatId::new(ChatId::MAX_CHAT_ID + 1).is_valid());
        assert!(!ChatId::new(-1).is_valid());
        assert!(!ChatId::new(i64::MIN).is_valid());
        assert_eq!(ChatId::new(123).get(), 123);
    }

    #[test]
    fn channel_id_bounds_match_the_cpp() {
        assert_eq!(ChannelId::MAX_CHANNEL_ID, 997_852_516_352);
        assert_eq!(ChannelId::MIN_MONOFORUM_CHANNEL_ID, 1_002_147_483_649);
        assert_eq!(ChannelId::MAX_MONOFORUM_CHANNEL_ID, 3_000_000_000_000);
        // the regular band is half-open below MAX_CHANNEL_ID
        assert!(!ChannelId::new(0).is_valid());
        assert!(ChannelId::new(1).is_valid());
        assert!(ChannelId::new(ChannelId::MAX_CHANNEL_ID - 1).is_valid());
        assert!(!ChannelId::new(ChannelId::MAX_CHANNEL_ID).is_valid());
        // the reserved secret-chat band between the two ranges
        assert!(!ChannelId::new(ChannelId::MAX_CHANNEL_ID + 1).is_valid());
        assert!(!ChannelId::new(ChannelId::MIN_MONOFORUM_CHANNEL_ID - 1).is_valid());
        // the monoforum band is [MIN, MAX)
        assert!(ChannelId::new(ChannelId::MIN_MONOFORUM_CHANNEL_ID).is_valid());
        assert!(ChannelId::new(ChannelId::MAX_MONOFORUM_CHANNEL_ID - 1).is_valid());
        assert!(!ChannelId::new(ChannelId::MAX_MONOFORUM_CHANNEL_ID).is_valid());
        assert!(!ChannelId::new(-1).is_valid());
    }

    #[test]
    fn channel_id_regular_predicate_stops_below_max() {
        assert!(ChannelId::new(1).is_regular_channel());
        assert!(ChannelId::new(ChannelId::MAX_CHANNEL_ID - 1).is_regular_channel());
        assert!(!ChannelId::new(ChannelId::MAX_CHANNEL_ID).is_regular_channel());
        assert!(!ChannelId::new(ChannelId::MIN_MONOFORUM_CHANNEL_ID).is_regular_channel());
    }

    #[test]
    fn enum_discriminants_index_the_pmc_keys() {
        assert_eq!(ChannelType::Broadcast as u8, 0);
        assert_eq!(ChannelType::Megagroup as u8, 1);
        assert_eq!(ChannelType::Unknown as u8, 2);
        assert_eq!(PublicDialogType::HasUsername as i32, 0);
        assert_eq!(PublicDialogType::IsLocationBased as i32, 1);
        assert_eq!(PublicDialogType::ForPersonalDialog as i32, 2);
    }

    #[test]
    fn dc_id_raw_gate_and_sentinels_match_the_cpp() {
        assert_eq!(DcId::MAX_RAW_DC_ID, 1000);
        assert!(!DcId::is_valid_raw(0));
        assert!(DcId::is_valid_raw(1));
        assert!(DcId::is_valid_raw(1000));
        assert!(!DcId::is_valid_raw(1001));
        assert!(!DcId::is_valid_raw(-1));

        assert!(DcId::empty().is_empty());
        assert!(!DcId::empty().is_exact());
        assert!(!DcId::empty().is_main());
        assert!(DcId::main().is_main());
        assert!(!DcId::main().is_empty());
        assert!(!DcId::main().is_exact());
        assert_eq!(DcId::main().get_value(), -1);
        assert!(DcId::invalid().is_empty());
        assert!(!DcId::invalid().is_main());
        assert_eq!(DcId::invalid().get_value(), -2);
    }

    #[test]
    fn dc_id_constructors_and_equality_match_the_cpp() {
        assert_eq!(DcId::create(2), DcId::internal(2));
        assert_eq!(DcId::create(0), DcId::invalid());
        assert_eq!(DcId::create(1001), DcId::invalid());
        assert_eq!(DcId::from_value(5), DcId::internal(5));
        assert_eq!(DcId::internal(2).get_raw_id(), 2);
        assert_eq!(DcId::internal(2).get_value(), 2);
        assert!(DcId::internal(2).is_internal());
        assert!(!DcId::internal(2).is_external());
        assert!(DcId::external(2).is_external());
        // equality pairs the raw id with the external marker
        assert_ne!(DcId::internal(2), DcId::external(2));
        assert_ne!(DcId::from_value(2), DcId::external(2));
        // operator< ties (dc_id, is_external)
        assert!(DcId::internal(2) < DcId::external(2));
        assert!(DcId::main() < DcId::internal(2));
    }

    #[test]
    #[should_panic]
    fn dc_id_get_raw_id_checks_exactness() {
        let _ = DcId::main().get_raw_id();
    }

    #[test]
    fn file_source_and_sticker_set_id_validity() {
        assert!(!FileSourceId::new(0).is_valid());
        assert!(FileSourceId::new(1).is_valid());
        assert!(!FileSourceId::new(-1).is_valid());
        assert_eq!(FileSourceId::new(7).get(), 7);

        assert!(!StickerSetId::new(0).is_valid());
        assert!(StickerSetId::new(1).is_valid());
        assert!(StickerSetId::new(-1).is_valid());
        assert!(StickerSetId::new(i64::MIN).is_valid());
        assert_eq!(StickerSetId::new(-5).get(), -5);
    }

    #[test]
    fn message_id_is_opaque_until_the_messages_step() {
        assert_eq!(MessageId::default().get(), 0);
        assert_eq!(MessageId::new(-7).get(), -7);
        assert_eq!(MessageId::new(1 << 21).get(), 1 << 21);
    }

    // --- M.5.2 RestrictedRights + AdministratorRights ---

    fn admin_from_all_flags() -> AdministratorRights {
        AdministratorRights::from_flags(u64::MAX)
    }

    #[test]
    fn administrator_mask_constants_match_the_cpp() {
        // DialogParticipant.h:28-48 — the server-side bit layout, pinned.
        assert_eq!(AdministratorRights::CAN_CHANGE_INFO_AND_SETTINGS, 1 << 0);
        assert_eq!(AdministratorRights::CAN_POST_MESSAGES, 1 << 1);
        assert_eq!(AdministratorRights::CAN_EDIT_MESSAGES, 1 << 2);
        assert_eq!(AdministratorRights::CAN_DELETE_MESSAGES, 1 << 3);
        assert_eq!(AdministratorRights::CAN_INVITE_USERS, 1 << 4);
        assert_eq!(AdministratorRights::CAN_RESTRICT_MEMBERS, 1 << 6);
        assert_eq!(AdministratorRights::CAN_PIN_MESSAGES, 1 << 7);
        assert_eq!(AdministratorRights::CAN_PROMOTE_MEMBERS, 1 << 8);
        assert_eq!(AdministratorRights::CAN_MANAGE_CALLS, 1 << 9);
        assert_eq!(AdministratorRights::CAN_MANAGE_DIALOG, 1 << 10);
        assert_eq!(AdministratorRights::CAN_MANAGE_TOPICS, 1 << 11);
        assert_eq!(AdministratorRights::IS_ANONYMOUS, 1 << 13);
        assert_eq!(AdministratorRights::LEGACY_CAN_SEND_MEDIA, 1 << 17);
        assert_eq!(AdministratorRights::CAN_POST_STORIES, 1 << 48);
        assert_eq!(AdministratorRights::CAN_EDIT_STORIES, 1 << 49);
        assert_eq!(AdministratorRights::CAN_DELETE_STORIES, 1 << 50);
        assert_eq!(AdministratorRights::CAN_MANAGE_DIRECT_MESSAGES, 1 << 51);
        assert_eq!(AdministratorRights::CAN_MANAGE_RANKS, 1 << 52);
        assert_eq!(AdministratorRights::CAN_MANAGE_LINKED_PEERS, 1 << 53);
        assert_eq!(AdministratorRights::CAN_MANAGE_WELCOME_MESSAGES, 1 << 54);
    }

    #[test]
    fn administrator_all_mask_is_the_server_parity_or() {
        // The C++ ALL_ADMINISTRATOR_RIGHTS composition: every admin bit
        // except IS_ANONYMOUS (which travels separately) and the legacy bit.
        let all = AdministratorRights::CAN_CHANGE_INFO_AND_SETTINGS
            | AdministratorRights::CAN_POST_MESSAGES
            | AdministratorRights::CAN_EDIT_MESSAGES
            | AdministratorRights::CAN_DELETE_MESSAGES
            | AdministratorRights::CAN_INVITE_USERS
            | AdministratorRights::CAN_RESTRICT_MEMBERS
            | AdministratorRights::CAN_PIN_MESSAGES
            | AdministratorRights::CAN_MANAGE_TOPICS
            | AdministratorRights::CAN_PROMOTE_MEMBERS
            | AdministratorRights::CAN_MANAGE_CALLS
            | AdministratorRights::CAN_MANAGE_DIALOG
            | AdministratorRights::CAN_POST_STORIES
            | AdministratorRights::CAN_EDIT_STORIES
            | AdministratorRights::CAN_DELETE_STORIES
            | AdministratorRights::CAN_MANAGE_DIRECT_MESSAGES
            | AdministratorRights::CAN_MANAGE_RANKS
            | AdministratorRights::CAN_MANAGE_LINKED_PEERS
            | AdministratorRights::CAN_MANAGE_WELCOME_MESSAGES;
        assert_eq!(AdministratorRights::ALL_ADMINISTRATOR_RIGHTS, all);
        assert_eq!(
            AdministratorRights::ALL_ADMINISTRATOR_RIGHTS & AdministratorRights::IS_ANONYMOUS,
            0
        );
        assert_eq!(
            AdministratorRights::ALL_ADMINISTRATOR_RIGHTS
                & AdministratorRights::LEGACY_CAN_SEND_MEDIA,
            0
        );
    }

    #[test]
    fn administrator_from_flags_masks_unknown_bits() {
        assert_eq!(
            admin_from_all_flags(),
            AdministratorRights::from_flags(
                AdministratorRights::ALL_ADMINISTRATOR_RIGHTS | AdministratorRights::IS_ANONYMOUS
            )
        );
        assert_eq!(
            AdministratorRights::from_flags(0),
            AdministratorRights::default()
        );
    }

    #[test]
    fn administrator_bool_ctor_zeroing_matrix() {
        let a = AdministratorRights::from_bools(
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            ChannelType::Broadcast,
        );
        // Broadcast zeroes pin/topics/ranks/anonymous only.
        assert!(!a.can_pin_messages());
        assert!(!a.can_manage_topics());
        assert!(!a.can_manage_ranks());
        assert!(!a.is_anonymous());
        assert!(a.can_post_messages());
        assert!(a.can_edit_messages());
        assert!(a.can_manage_direct_messages());
        assert!(a.can_manage_dialog());

        let b = AdministratorRights::from_bools(
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            ChannelType::Megagroup,
        );
        // Megagroup zeroes post/edit/direct-messages only.
        assert!(!b.can_post_messages());
        assert!(!b.can_edit_messages());
        assert!(!b.can_manage_direct_messages());
        assert!(b.can_pin_messages());
        assert!(b.can_manage_topics());
        assert!(b.can_manage_ranks());
        assert!(b.is_anonymous());

        let c = AdministratorRights::from_bools(
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            ChannelType::Unknown,
        );
        // Unknown zeroes topics only.
        assert!(!c.can_manage_topics());
        assert!(c.can_pin_messages());
        assert!(c.can_manage_ranks());
        assert!(c.is_anonymous());

        // The all-false set is not upgraded with CAN_MANAGE_DIALOG.
        let d = AdministratorRights::from_bools(
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            ChannelType::Megagroup,
        );
        assert_eq!(d, AdministratorRights::default());
        assert!(!d.can_manage_dialog());
    }

    #[test]
    fn administrator_recorded_chat_admin_rights_vectors() {
        // Recorded telegram_api::chatAdminRights payloads (server-shaped) with
        // the flags the C++ ctor derives, hand-computed from DialogParticipant.cpp:21-42.

        // Vector 1 — a full broadcast administrator.
        let broadcast_full = telegram_api::ChatAdminRights {
            flags: 0,
            change_info: true,
            post_messages: true,
            edit_messages: true,
            delete_messages: true,
            ban_users: true,
            invite_users: true,
            pin_messages: true,
            add_admins: true,
            anonymous: true,
            manage_call: true,
            other: true,
            manage_topics: true,
            post_stories: true,
            edit_stories: true,
            delete_stories: true,
            manage_direct_messages: true,
            manage_ranks: true,
            manage_linked_peers: true,
            manage_welcome_messages: true,
        };
        let rights = AdministratorRights::from_chat_admin_rights(
            Some(&broadcast_full),
            ChannelType::Broadcast,
        );
        let expected = AdministratorRights::from_flags(
            AdministratorRights::CAN_MANAGE_DIALOG
                | AdministratorRights::CAN_CHANGE_INFO_AND_SETTINGS
                | AdministratorRights::CAN_POST_MESSAGES
                | AdministratorRights::CAN_EDIT_MESSAGES
                | AdministratorRights::CAN_DELETE_MESSAGES
                | AdministratorRights::CAN_INVITE_USERS
                | AdministratorRights::CAN_RESTRICT_MEMBERS
                | AdministratorRights::CAN_PROMOTE_MEMBERS
                | AdministratorRights::CAN_MANAGE_CALLS
                | AdministratorRights::CAN_POST_STORIES
                | AdministratorRights::CAN_EDIT_STORIES
                | AdministratorRights::CAN_DELETE_STORIES
                | AdministratorRights::CAN_MANAGE_DIRECT_MESSAGES
                | AdministratorRights::CAN_MANAGE_LINKED_PEERS
                | AdministratorRights::CAN_MANAGE_WELCOME_MESSAGES,
        );
        assert_eq!(rights, expected);
        assert!(!rights.can_pin_messages()); // zeroed for broadcasts
        assert!(!rights.can_manage_topics()); // zeroed for broadcasts
        assert!(!rights.can_manage_ranks()); // zeroed for broadcasts
        assert!(!rights.is_anonymous()); // zeroed for broadcasts

        // Vector 2 — a megagroup administrator with post/edit/direct set: the
        // zeroing drops those three but keeps ranks and anonymous.
        let megagroup = telegram_api::ChatAdminRights {
            flags: 0,
            change_info: true,
            post_messages: true,
            edit_messages: true,
            delete_messages: false,
            ban_users: false,
            invite_users: false,
            pin_messages: true,
            add_admins: false,
            anonymous: true,
            manage_call: false,
            other: true,
            manage_topics: true,
            post_stories: false,
            edit_stories: false,
            delete_stories: false,
            manage_direct_messages: true,
            manage_ranks: true,
            manage_linked_peers: false,
            manage_welcome_messages: false,
        };
        let rights =
            AdministratorRights::from_chat_admin_rights(Some(&megagroup), ChannelType::Megagroup);
        let expected = AdministratorRights::from_flags(
            AdministratorRights::CAN_MANAGE_DIALOG
                | AdministratorRights::CAN_CHANGE_INFO_AND_SETTINGS
                | AdministratorRights::CAN_PIN_MESSAGES
                | AdministratorRights::CAN_MANAGE_TOPICS
                | AdministratorRights::CAN_MANAGE_RANKS
                | AdministratorRights::IS_ANONYMOUS,
        );
        assert_eq!(rights, expected);
        assert!(!rights.can_post_messages());
        assert!(!rights.can_edit_messages());
        assert!(!rights.can_manage_direct_messages());
        assert!(rights.can_manage_ranks());

        // Vector 3 — other=false with an unknown channel type: the C++ turns
        // `other` on instead of erroring, so CAN_MANAGE_DIALOG appears.
        let unknown = telegram_api::ChatAdminRights {
            flags: 0,
            change_info: true,
            post_messages: false,
            edit_messages: false,
            delete_messages: false,
            ban_users: false,
            invite_users: false,
            pin_messages: false,
            add_admins: false,
            anonymous: false,
            manage_call: false,
            other: false,
            manage_topics: true,
            post_stories: false,
            edit_stories: false,
            delete_stories: false,
            manage_direct_messages: false,
            manage_ranks: false,
            manage_linked_peers: false,
            manage_welcome_messages: false,
        };
        let rights =
            AdministratorRights::from_chat_admin_rights(Some(&unknown), ChannelType::Unknown);
        let expected = AdministratorRights::from_flags(
            AdministratorRights::CAN_MANAGE_DIALOG
                | AdministratorRights::CAN_CHANGE_INFO_AND_SETTINGS,
        );
        assert_eq!(rights, expected);
        assert!(!rights.can_manage_topics()); // zeroed for unknown

        // Vector 4 — other=false with a known channel type and no rights at
        // all: the wrong-other error path leaves the set empty.
        let empty = telegram_api::ChatAdminRights::default();
        let rights =
            AdministratorRights::from_chat_admin_rights(Some(&empty), ChannelType::Megagroup);
        assert_eq!(rights, AdministratorRights::default());

        // A null pointer is the empty set.
        assert_eq!(
            AdministratorRights::from_chat_admin_rights(None, ChannelType::Megagroup),
            AdministratorRights::default()
        );
    }

    #[test]
    fn administrator_get_chat_admin_rights_round_trip() {
        // Flags survive rights -> chatAdminRights -> rights for every set the
        // channel type can represent (other = can_manage_dialog is auto-set
        // for non-empty sets; each channel type zeroes its impossible bits,
        // so the tested sets avoid those).
        let megagroup_sets = [
            AdministratorRights::from_flags(AdministratorRights::CAN_MANAGE_DIALOG),
            AdministratorRights::from_flags(
                AdministratorRights::CAN_MANAGE_DIALOG
                    | AdministratorRights::CAN_CHANGE_INFO_AND_SETTINGS
                    | AdministratorRights::CAN_INVITE_USERS,
            ),
            AdministratorRights::from_flags(
                AdministratorRights::ALL_ADMINISTRATOR_RIGHTS
                    & !(AdministratorRights::CAN_POST_MESSAGES
                        | AdministratorRights::CAN_EDIT_MESSAGES
                        | AdministratorRights::CAN_MANAGE_DIRECT_MESSAGES),
            ),
            AdministratorRights::from_flags(
                (AdministratorRights::ALL_ADMINISTRATOR_RIGHTS
                    & !(AdministratorRights::CAN_POST_MESSAGES
                        | AdministratorRights::CAN_EDIT_MESSAGES
                        | AdministratorRights::CAN_MANAGE_DIRECT_MESSAGES))
                    | AdministratorRights::IS_ANONYMOUS,
            ),
        ];
        for rights in megagroup_sets {
            let object = rights.get_chat_admin_rights();
            assert_eq!(
                AdministratorRights::from_chat_admin_rights(Some(&object), ChannelType::Megagroup),
                rights
            );
        }

        // An unknown channel type keeps everything but manage_topics.
        let rights = AdministratorRights::from_flags(
            AdministratorRights::ALL_ADMINISTRATOR_RIGHTS & !AdministratorRights::CAN_MANAGE_TOPICS,
        );
        let object = rights.get_chat_admin_rights();
        assert_eq!(
            AdministratorRights::from_chat_admin_rights(Some(&object), ChannelType::Unknown),
            rights
        );

        // The empty set round-trips through a megagroup (the wrong-other
        // error path keeps `other` false), and a null object is empty too.
        let object = AdministratorRights::default().get_chat_admin_rights();
        assert_eq!(
            AdministratorRights::from_chat_admin_rights(Some(&object), ChannelType::Megagroup),
            AdministratorRights::default()
        );
        assert_eq!(
            AdministratorRights::from_chat_admin_rights(None, ChannelType::Megagroup),
            AdministratorRights::default()
        );
    }

    #[test]
    fn administrator_td_api_ctor_and_object_conversions() {
        // Recorded td_api::chatAdministratorRights payload: the C++ mapping
        // (can_manage_video_chats -> CAN_MANAGE_CALLS, can_manage_tags ->
        // CAN_MANAGE_RANKS, linked peers hard-false).
        let api = td_api::ChatAdministratorRights {
            can_manage_chat: true,
            can_change_info: true,
            can_post_messages: false,
            can_edit_messages: false,
            can_delete_messages: false,
            can_invite_users: false,
            can_restrict_members: true,
            can_pin_messages: false,
            can_manage_topics: false,
            can_promote_members: true,
            can_manage_video_chats: true,
            can_post_stories: false,
            can_edit_stories: false,
            can_delete_stories: false,
            can_manage_direct_messages: false,
            can_manage_tags: true,
            can_send_welcome_messages: false,
            is_anonymous: true,
        };
        let rights =
            AdministratorRights::from_chat_administrator_rights(Some(&api), ChannelType::Megagroup);
        let expected = AdministratorRights::from_flags(
            AdministratorRights::CAN_MANAGE_DIALOG
                | AdministratorRights::CAN_CHANGE_INFO_AND_SETTINGS
                | AdministratorRights::CAN_RESTRICT_MEMBERS
                | AdministratorRights::CAN_PROMOTE_MEMBERS
                | AdministratorRights::CAN_MANAGE_CALLS
                | AdministratorRights::CAN_MANAGE_RANKS
                | AdministratorRights::IS_ANONYMOUS,
        );
        assert_eq!(rights, expected);

        // The object getter mirrors the flag set.
        let object = rights.get_chat_administrator_rights_object();
        assert!(object.can_manage_chat);
        assert!(object.can_change_info);
        assert!(object.can_restrict_members);
        assert!(object.can_promote_members);
        assert!(object.can_manage_video_chats);
        assert!(object.can_manage_tags);
        assert!(object.is_anonymous);
        assert!(!object.can_post_messages);
        assert!(!object.can_manage_topics);

        // Recorded td_api::communityAdministratorRights payload.
        let community = td_api::CommunityAdministratorRights {
            can_manage_community: true,
            can_change_info: true,
            can_edit_chat_list: true,
            can_promote_members: true,
            can_ban_members: true,
        };
        let rights = AdministratorRights::from_community_administrator_rights(Some(&community));
        let expected = AdministratorRights::from_flags(
            AdministratorRights::CAN_MANAGE_DIALOG
                | AdministratorRights::CAN_CHANGE_INFO_AND_SETTINGS
                | AdministratorRights::CAN_RESTRICT_MEMBERS
                | AdministratorRights::CAN_PROMOTE_MEMBERS
                | AdministratorRights::CAN_MANAGE_LINKED_PEERS,
        );
        assert_eq!(rights, expected);
        assert_eq!(
            AdministratorRights::from_community_administrator_rights(None),
            AdministratorRights::default()
        );

        let community_object = rights.get_community_administrator_rights_object();
        assert!(community_object.can_manage_community);
        assert!(community_object.can_change_info);
        assert!(community_object.can_edit_chat_list);
        assert!(community_object.can_promote_members);
        assert!(community_object.can_ban_members);
    }

    #[test]
    fn restricted_mask_constants_match_the_cpp() {
        // DialogParticipant.h:193-216 — the server-side bit layout, pinned.
        assert_eq!(RestrictedRights::CAN_SEND_MESSAGES, 1 << 16);
        assert_eq!(RestrictedRights::LEGACY_CAN_SEND_MEDIA, 1 << 17);
        assert_eq!(RestrictedRights::CAN_SEND_AUDIOS, 1 << 32);
        assert_eq!(RestrictedRights::CAN_SEND_DOCUMENTS, 1 << 33);
        assert_eq!(RestrictedRights::CAN_SEND_PHOTOS, 1 << 34);
        assert_eq!(RestrictedRights::CAN_SEND_VIDEOS, 1 << 35);
        assert_eq!(RestrictedRights::CAN_SEND_VIDEO_NOTES, 1 << 36);
        assert_eq!(RestrictedRights::CAN_SEND_VOICE_NOTES, 1 << 37);
        assert_eq!(RestrictedRights::CAN_SEND_STICKERS, 1 << 18);
        assert_eq!(RestrictedRights::CAN_SEND_ANIMATIONS, 1 << 19);
        assert_eq!(RestrictedRights::CAN_SEND_GAMES, 1 << 20);
        assert_eq!(RestrictedRights::CAN_USE_INLINE_BOTS, 1 << 21);
        assert_eq!(RestrictedRights::CAN_ADD_WEB_PAGE_PREVIEWS, 1 << 22);
        assert_eq!(RestrictedRights::CAN_SEND_POLLS, 1 << 23);
        assert_eq!(RestrictedRights::CAN_CHANGE_INFO_AND_SETTINGS, 1 << 24);
        assert_eq!(RestrictedRights::CAN_INVITE_USERS, 1 << 25);
        assert_eq!(RestrictedRights::CAN_PIN_MESSAGES, 1 << 26);
        assert_eq!(RestrictedRights::CAN_MANAGE_TOPICS, 1 << 12);
        assert_eq!(RestrictedRights::CAN_EDIT_RANK, 1 << 38);
        assert_eq!(RestrictedRights::CAN_SEND_REACTIONS, 1 << 39);
        assert_eq!(RestrictedRights::CAN_MANAGE_LINKED_PEERS, 1 << 40);
    }

    #[test]
    fn restricted_composed_masks_match_the_cpp() {
        let admin_permission = RestrictedRights::CAN_CHANGE_INFO_AND_SETTINGS
            | RestrictedRights::CAN_INVITE_USERS
            | RestrictedRights::CAN_PIN_MESSAGES
            | RestrictedRights::CAN_MANAGE_TOPICS
            | RestrictedRights::CAN_MANAGE_LINKED_PEERS;
        assert_eq!(
            RestrictedRights::ALL_ADMIN_PERMISSION_RIGHTS,
            admin_permission
        );

        let all = RestrictedRights::CAN_SEND_MESSAGES
            | RestrictedRights::CAN_SEND_STICKERS
            | RestrictedRights::CAN_SEND_ANIMATIONS
            | RestrictedRights::CAN_SEND_GAMES
            | RestrictedRights::CAN_USE_INLINE_BOTS
            | RestrictedRights::CAN_ADD_WEB_PAGE_PREVIEWS
            | RestrictedRights::CAN_SEND_POLLS
            | RestrictedRights::ALL_ADMIN_PERMISSION_RIGHTS
            | RestrictedRights::CAN_SEND_AUDIOS
            | RestrictedRights::CAN_SEND_DOCUMENTS
            | RestrictedRights::CAN_SEND_PHOTOS
            | RestrictedRights::CAN_SEND_VIDEOS
            | RestrictedRights::CAN_SEND_VIDEO_NOTES
            | RestrictedRights::CAN_SEND_VOICE_NOTES
            | RestrictedRights::CAN_EDIT_RANK
            | RestrictedRights::CAN_SEND_REACTIONS;
        assert_eq!(RestrictedRights::ALL_RESTRICTED_RIGHTS, all);
        assert_eq!(
            RestrictedRights::ALL_RESTRICTED_RIGHTS & RestrictedRights::LEGACY_CAN_SEND_MEDIA,
            0
        );
    }

    #[test]
    fn restricted_from_flags_masks_unknown_bits() {
        assert_eq!(
            RestrictedRights::from_flags(u64::MAX),
            RestrictedRights::from_flags(RestrictedRights::ALL_RESTRICTED_RIGHTS)
        );
        assert_eq!(RestrictedRights::from_flags(0), RestrictedRights::default());
    }

    #[test]
    fn restricted_bool_ctor_and_broadcast_zeroing() {
        // A broadcast yields the empty set whatever the input.
        let broadcast = RestrictedRights::from_bools(
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            ChannelType::Broadcast,
        );
        assert_eq!(broadcast, RestrictedRights::default());

        // Otherwise every true input lands its bit.
        let rights = RestrictedRights::from_bools(
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            ChannelType::Megagroup,
        );
        assert_eq!(
            rights,
            RestrictedRights::from_flags(RestrictedRights::ALL_RESTRICTED_RIGHTS)
        );
    }

    #[test]
    fn restricted_recorded_chat_banned_rights_vectors() {
        // Recorded telegram_api::chatBannedRights payloads (server-shaped)
        // with the flags the C++ ctor derives, hand-computed from
        // DialogParticipant.cpp:211-229.

        // Vector 1 — a member banned from plain text, photos, polls and
        // changing the chat info: everything else stays allowed.
        let partial = telegram_api::ChatBannedRights {
            flags: 0,
            view_messages: false,
            send_messages: false,
            send_media: false,
            send_stickers: false,
            send_gifs: false,
            send_games: false,
            send_inline: false,
            embed_links: false,
            send_polls: true,
            change_info: true,
            invite_users: false,
            pin_messages: false,
            manage_topics: false,
            send_photos: true,
            send_videos: false,
            send_roundvideos: false,
            send_audios: false,
            send_voices: false,
            send_docs: false,
            send_plain: true,
            edit_rank: false,
            send_reactions: false,
            manage_linked_peers: false,
            until_date: i32::MAX,
        };
        let rights =
            RestrictedRights::from_chat_banned_rights(Some(&partial), ChannelType::Megagroup);
        let expected = RestrictedRights::from_flags(
            RestrictedRights::CAN_SEND_AUDIOS
                | RestrictedRights::CAN_SEND_DOCUMENTS
                | RestrictedRights::CAN_SEND_VIDEOS
                | RestrictedRights::CAN_SEND_VIDEO_NOTES
                | RestrictedRights::CAN_SEND_VOICE_NOTES
                | RestrictedRights::CAN_SEND_STICKERS
                | RestrictedRights::CAN_SEND_ANIMATIONS
                | RestrictedRights::CAN_SEND_GAMES
                | RestrictedRights::CAN_USE_INLINE_BOTS
                | RestrictedRights::CAN_ADD_WEB_PAGE_PREVIEWS
                | RestrictedRights::CAN_INVITE_USERS
                | RestrictedRights::CAN_PIN_MESSAGES
                | RestrictedRights::CAN_MANAGE_TOPICS
                | RestrictedRights::CAN_EDIT_RANK
                | RestrictedRights::CAN_SEND_REACTIONS
                | RestrictedRights::CAN_MANAGE_LINKED_PEERS,
        );
        assert_eq!(rights, expected);
        assert!(!rights.can_send_messages());
        assert!(!rights.can_send_photos());
        assert!(!rights.can_send_polls());
        assert!(!rights.can_change_info_and_settings());

        // Vector 2 — banned from everything: the empty permission set. The
        // stray view_messages/until_date fields hit the C++ error logs but do
        // not change the mapping.
        let mut full = partial;
        full.view_messages = true;
        full.until_date = 12345;
        full.send_stickers = true;
        full.send_gifs = true;
        full.send_games = true;
        full.send_inline = true;
        full.embed_links = true;
        full.invite_users = true;
        full.pin_messages = true;
        full.manage_topics = true;
        full.send_videos = true;
        full.send_roundvideos = true;
        full.send_audios = true;
        full.send_voices = true;
        full.send_docs = true;
        full.edit_rank = true;
        full.send_reactions = true;
        full.manage_linked_peers = true;
        let rights = RestrictedRights::from_chat_banned_rights(Some(&full), ChannelType::Megagroup);
        assert_eq!(rights, RestrictedRights::default());

        // Vector 3 — the same bans for a broadcast channel are ignored.
        let rights = RestrictedRights::from_chat_banned_rights(Some(&full), ChannelType::Broadcast);
        assert_eq!(rights, RestrictedRights::default());

        // A null pointer is the empty set.
        assert_eq!(
            RestrictedRights::from_chat_banned_rights(None, ChannelType::Megagroup),
            RestrictedRights::default()
        );
    }

    #[test]
    fn restricted_get_chat_banned_rights_round_trip() {
        let base = [
            RestrictedRights::from_flags(0),
            RestrictedRights::from_flags(RestrictedRights::CAN_SEND_MESSAGES),
            RestrictedRights::from_flags(
                RestrictedRights::CAN_SEND_MESSAGES
                    | RestrictedRights::CAN_CHANGE_INFO_AND_SETTINGS
                    | RestrictedRights::CAN_MANAGE_LINKED_PEERS,
            ),
            RestrictedRights::from_flags(RestrictedRights::ALL_RESTRICTED_RIGHTS),
        ];
        for rights in base {
            let object = rights.get_chat_banned_rights();
            assert_eq!(
                RestrictedRights::from_chat_banned_rights(Some(&object), ChannelType::Megagroup),
                rights
            );
        }
    }

    #[test]
    fn restricted_td_api_ctor_and_object_conversions() {
        // Recorded td_api::chatPermissions payload: the four
        // can_send_other_messages consumers, create_topics -> manage_topics,
        // edit_tag -> edit_rank and the hard-true linked peers.
        let api = td_api::ChatPermissions {
            can_send_basic_messages: true,
            can_send_audios: false,
            can_send_documents: true,
            can_send_photos: false,
            can_send_videos: true,
            can_send_video_notes: false,
            can_send_voice_notes: true,
            can_send_polls: false,
            can_send_other_messages: true,
            can_add_link_previews: false,
            can_react_to_messages: true,
            can_edit_tag: false,
            can_change_info: true,
            can_invite_users: false,
            can_pin_messages: true,
            can_create_topics: false,
        };
        let rights = RestrictedRights::from_chat_permissions(Some(&api), ChannelType::Megagroup);
        let expected = RestrictedRights::from_flags(
            RestrictedRights::CAN_SEND_MESSAGES
                | RestrictedRights::CAN_SEND_DOCUMENTS
                | RestrictedRights::CAN_SEND_VIDEOS
                | RestrictedRights::CAN_SEND_VOICE_NOTES
                | RestrictedRights::CAN_SEND_STICKERS
                | RestrictedRights::CAN_SEND_ANIMATIONS
                | RestrictedRights::CAN_SEND_GAMES
                | RestrictedRights::CAN_USE_INLINE_BOTS
                | RestrictedRights::CAN_CHANGE_INFO_AND_SETTINGS
                | RestrictedRights::CAN_PIN_MESSAGES
                | RestrictedRights::CAN_SEND_REACTIONS
                | RestrictedRights::CAN_MANAGE_LINKED_PEERS,
        );
        assert_eq!(rights, expected);

        // Broadcast permissions are ignored.
        assert_eq!(
            RestrictedRights::from_chat_permissions(Some(&api), ChannelType::Broadcast),
            RestrictedRights::default()
        );

        // The object getter mirrors the set, with the four media-adjacent
        // flags collapsed into can_send_other_messages.
        let object = rights.get_chat_permissions_object();
        assert!(object.can_send_basic_messages);
        assert!(!object.can_send_audios);
        assert!(object.can_send_documents);
        assert!(!object.can_send_photos);
        assert!(object.can_send_videos);
        assert!(!object.can_send_video_notes);
        assert!(object.can_send_voice_notes);
        assert!(!object.can_send_polls);
        assert!(object.can_send_other_messages);
        assert!(!object.can_add_link_previews);
        assert!(object.can_react_to_messages);
        assert!(!object.can_edit_tag);
        assert!(object.can_change_info);
        assert!(!object.can_invite_users);
        assert!(object.can_pin_messages);
        assert!(!object.can_create_topics);

        // Recorded td_api::communityPermissions payload.
        let community = td_api::CommunityPermissions {
            can_edit_chat_list: true,
        };
        let rights = RestrictedRights::from_community_permissions(Some(&community));
        let expected = RestrictedRights::from_flags(RestrictedRights::CAN_MANAGE_LINKED_PEERS);
        assert_eq!(rights, expected);
        assert_eq!(
            RestrictedRights::from_community_permissions(None),
            RestrictedRights::default()
        );
        assert!(rights.get_community_permissions_object().can_edit_chat_list);

        // restrict_all is the empty permission set, built through the C++
        // null-chatPermissions route.
        assert_eq!(
            RestrictedRights::restrict_all(),
            RestrictedRights::default()
        );
        assert!(!RestrictedRights::restrict_all().can_send_messages());
        assert!(!RestrictedRights::restrict_all().can_manage_linked_peers());
    }

    #[test]
    fn rights_versioned_store_parse_framing() {
        // The 64-bit flags serialize as a raw little-endian uint64.
        let rights = RestrictedRights::from_flags(RestrictedRights::ALL_RESTRICTED_RIGHTS);
        let mut out = Vec::new();
        rights.store(&mut out);
        assert_eq!(out.len(), 8);
        assert_eq!(out, (RestrictedRights::ALL_RESTRICTED_RIGHTS).to_le_bytes());
        let mut parser = Parser::new(&out);
        assert_eq!(RestrictedRights::parse_impl(&mut parser).unwrap(), rights);
        parser.fetch_end().unwrap();

        // The legacy (pre-MakeParticipantFlags64Bit) framing reads a uint32;
        // the log-event parser carries the version prefix.
        let mut legacy = Vec::new();
        legacy.extend_from_slice(&45i32.to_le_bytes());
        legacy.extend_from_slice(&(RestrictedRights::CAN_SEND_MESSAGES as u32).to_le_bytes());
        let mut parser = Parser::new_log_event(&legacy).unwrap();
        assert_eq!(
            RestrictedRights::parse_impl(&mut parser).unwrap(),
            RestrictedRights::from_flags(RestrictedRights::CAN_SEND_MESSAGES)
        );
        parser.fetch_end().unwrap();

        // The legacy CAN_SEND_MEDIA bit expands into the six per-media bits;
        // the C++ parse keeps the legacy bit itself (no masking).
        let mut legacy_media = Vec::new();
        legacy_media.extend_from_slice(&45i32.to_le_bytes());
        legacy_media
            .extend_from_slice(&(RestrictedRights::LEGACY_CAN_SEND_MEDIA as u32).to_le_bytes());
        let mut parser = Parser::new_log_event(&legacy_media).unwrap();
        let expanded = RestrictedRights::parse_impl(&mut parser).unwrap();
        assert_eq!(
            expanded.flags,
            RestrictedRights::LEGACY_CAN_SEND_MEDIA
                | RestrictedRights::CAN_SEND_AUDIOS
                | RestrictedRights::CAN_SEND_DOCUMENTS
                | RestrictedRights::CAN_SEND_PHOTOS
                | RestrictedRights::CAN_SEND_VIDEOS
                | RestrictedRights::CAN_SEND_VIDEO_NOTES
                | RestrictedRights::CAN_SEND_VOICE_NOTES
        );
        assert!(expanded.can_send_photos());
        assert!(expanded.can_send_voice_notes());

        // AdministratorRights uses the same versioned framing without the
        // legacy expansion.
        let admin = AdministratorRights::from_flags(AdministratorRights::ALL_ADMINISTRATOR_RIGHTS);
        let mut out = Vec::new();
        admin.store(&mut out);
        assert_eq!(
            out,
            (AdministratorRights::ALL_ADMINISTRATOR_RIGHTS).to_le_bytes()
        );
        let mut parser = Parser::new(&out);
        assert_eq!(AdministratorRights::parse_impl(&mut parser).unwrap(), admin);
        parser.fetch_end().unwrap();

        let mut legacy_admin = Vec::new();
        legacy_admin.extend_from_slice(&45i32.to_le_bytes());
        legacy_admin
            .extend_from_slice(&(AdministratorRights::CAN_MANAGE_DIALOG as u32).to_le_bytes());
        let mut parser = Parser::new_log_event(&legacy_admin).unwrap();
        assert_eq!(
            AdministratorRights::parse_impl(&mut parser).unwrap(),
            AdministratorRights::from_flags(AdministratorRights::CAN_MANAGE_DIALOG)
        );
        parser.fetch_end().unwrap();
    }

    // ---- DialogParticipantStatus (M.5.3) ----

    fn status_store_hex(status: &DialogParticipantStatus) -> String {
        let mut out = Vec::new();
        crate::store_parse::store_log_event_version(&mut out);
        status.store(&mut out);
        out.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn status_round_trip(status: &DialogParticipantStatus) -> DialogParticipantStatus {
        let mut out = Vec::new();
        crate::store_parse::store_log_event_version(&mut out);
        status.store(&mut out);
        let mut parser = Parser::new_log_event(&out).unwrap();
        let parsed = DialogParticipantStatus::parse_impl(&mut parser).unwrap();
        parser.fetch_end().unwrap();
        parsed
    }

    /// The `DialogParticipantStatus` predicate matrix, in the fixed order of
    /// `conformance/chat_participant_status_oracle/probe.cpp`'s `matrix()`.
    fn status_matrix(status: &DialogParticipantStatus) -> String {
        let preds = [
            status.is_creator(),
            status.is_administrator(),
            status.is_administrator_member(),
            status.is_member(),
            status.is_left(),
            status.is_restricted(),
            status.is_banned(),
            status.can_manage_dialog(),
            status.can_change_info_and_settings(),
            status.can_post_messages(),
            status.can_edit_messages(),
            status.can_delete_messages(),
            status.can_invite_users(),
            status.can_restrict_members(),
            status.can_pin_messages(),
            status.can_manage_invite_links(),
            status.can_promote_members(),
            status.can_manage_calls(),
            status.can_post_stories(),
            status.can_edit_stories(),
            status.can_delete_stories(),
            status.can_manage_direct_messages(),
            status.can_manage_ranks(),
            status.can_manage_linked_peers(),
            status.can_manage_welcome_messages(),
            status.can_edit_topics(),
            status.can_pin_topics(),
            status.can_create_topics(),
            status.can_be_edited(),
            status.can_send_messages(),
            status.can_send_audios(),
            status.can_send_documents(),
            status.can_send_photos(),
            status.can_send_videos(),
            status.can_send_video_notes(),
            status.can_send_voice_notes(),
            status.can_send_stickers(),
            status.can_send_animations(),
            status.can_send_games(),
            status.can_use_inline_bots(),
            status.can_add_web_page_previews(),
            status.can_send_polls(),
            status.can_edit_rank(),
            status.can_send_reactions(),
            status.has_all_administrator_rights(AdministratorRights::from_bools(
                false,
                true,
                true,
                false,
                false,
                true,
                true,
                true,
                true,
                false,
                false,
                true,
                false,
                false,
                false,
                false,
                true,
                false,
                true,
                ChannelType::Unknown,
            )),
        ];
        preds.iter().map(|p| if *p { '1' } else { '0' }).collect()
    }

    fn admin_full() -> AdministratorRights {
        AdministratorRights::from_bools(
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            ChannelType::Unknown,
        )
    }

    fn admin_edited() -> AdministratorRights {
        AdministratorRights::from_bools(
            false,
            true,
            false,
            false,
            true,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            ChannelType::Unknown,
        )
    }

    fn restricted_member_rights() -> RestrictedRights {
        RestrictedRights::from_bools(
            true,
            false,
            false,
            false,
            false,
            false,
            false,
            true,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            ChannelType::Megagroup,
        )
    }

    fn restricted_non_member_rights() -> RestrictedRights {
        RestrictedRights::from_bools(
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            true,
            false,
            false,
            false,
            false,
            false,
            false,
            ChannelType::Megagroup,
        )
    }

    /// All samples of the C++ oracle, as the oracle constructs them.
    fn oracle_samples() -> Vec<(&'static str, DialogParticipantStatus)> {
        vec![
            (
                "creator_member",
                DialogParticipantStatus::creator(true, false, String::new()),
            ),
            (
                "creator_member_anon_rank",
                DialogParticipantStatus::creator(true, true, "admin".to_owned()),
            ),
            (
                "creator_non_member",
                DialogParticipantStatus::creator(false, false, String::new()),
            ),
            (
                "administrator_full",
                DialogParticipantStatus::administrator(admin_full(), "boss".to_owned(), false),
            ),
            (
                "administrator_min",
                DialogParticipantStatus::administrator(
                    AdministratorRights::default(),
                    "x".to_owned(),
                    true,
                ),
            ),
            (
                "administrator_edited",
                DialogParticipantStatus::administrator(admin_edited(), "ed".to_owned(), true),
            ),
            (
                "member_until",
                DialogParticipantStatus::member(123456, "rank".to_owned()),
            ),
            (
                "member_plain",
                DialogParticipantStatus::member(0, String::new()),
            ),
            (
                "restricted_member",
                DialogParticipantStatus::restricted(
                    restricted_member_rights(),
                    true,
                    1000,
                    ChannelType::Megagroup,
                    "r".to_owned(),
                ),
            ),
            (
                "restricted_non_member",
                DialogParticipantStatus::restricted(
                    restricted_non_member_rights(),
                    false,
                    0,
                    ChannelType::Megagroup,
                    String::new(),
                ),
            ),
            (
                "restricted_all_broadcast",
                DialogParticipantStatus::restricted(
                    RestrictedRights::restrict_all(),
                    true,
                    0,
                    ChannelType::Broadcast,
                    String::new(),
                ),
            ),
            (
                "restricted_none_megagroup",
                DialogParticipantStatus::restricted(
                    RestrictedRights::restrict_all(),
                    true,
                    0,
                    ChannelType::Megagroup,
                    String::new(),
                ),
            ),
            ("left", DialogParticipantStatus::left()),
            (
                "banned_until",
                DialogParticipantStatus::banned(2000, "ban".to_owned()),
            ),
            (
                "banned_default",
                DialogParticipantStatus::banned(0, String::new()),
            ),
            (
                "banned_max",
                DialogParticipantStatus::banned(i32::MAX, String::new()),
            ),
            (
                "group_admin",
                DialogParticipantStatus::group_administrator(true, "gadm".to_owned()),
            ),
            (
                "channel_admin_megagroup",
                DialogParticipantStatus::channel_administrator(true, true),
            ),
            (
                "channel_admin_broadcast",
                DialogParticipantStatus::channel_administrator(false, false),
            ),
        ]
    }

    /// The stored bytes of every sample, pinned against the compiled C++
    /// (`conformance/chat_participant_status_oracle`): the 61 log-event
    /// version prefix plus the bare record body.
    #[test]
    fn status_store_bytes_match_the_cpp_probe() {
        let expected = [
            ("creator_member", "3d000000df1ffd0fff017f00"),
            (
                "creator_member_anon_rank",
                "3d000000df7ffd0fff017f000561646d696e0000",
            ),
            ("creator_non_member", "3d000000df1ffd07ff017f00"),
            (
                "administrator_full",
                "3d000000df67fd18ff007f0004626f7373000000",
            ),
            ("administrator_min", "3d0000000050fd2fff01000001780000"),
            ("administrator_edited", "3d00000004c4fd18ff00000002656400"),
            (
                "member_until",
                "3d0000000050fdafff01000040e201000472616e6b000000",
            ),
            ("member_plain", "3d0000000010fd2fff010000"),
            (
                "restricted_member",
                "3d000000004005b800000000e803000001720000",
            ),
            ("restricted_non_member", "3d0000000000003100000000"),
            ("restricted_all_broadcast", "3d0000000010fd2fff010000"),
            ("restricted_none_megagroup", "3d0000000000003800000000"),
            ("left", "3d0000000010fd47ff010000"),
            ("banned_until", "3d000000004000d000000000d00700000362616e"),
            ("banned_default", "3d0000000000005000000000"),
            ("banned_max", "3d0000000000005000000000"),
            ("group_admin", "3d000000d9c6fd18ff005000046761646d000000"),
            ("channel_admin_megagroup", "3d000000d98cfd18ff001000"),
            ("channel_admin_broadcast", "3d0000004e04fd18ff000f00"),
        ];
        let samples = oracle_samples();
        assert_eq!(samples.len(), expected.len());
        for ((name, status), (exp_name, exp_hex)) in samples.iter().zip(expected) {
            assert_eq!(*name, exp_name);
            assert_eq!(status_store_hex(status), exp_hex, "stored bytes of {name}");
        }
    }

    /// Store → parse → re-store must reproduce the bytes for every sample
    /// (the versioned framing survives the round trip), matching the C++
    /// oracle's own `rt:` self-consistency lines.
    #[test]
    fn status_round_trips_through_the_versioned_framing() {
        for (name, status) in oracle_samples() {
            let parsed = status_round_trip(&status);
            assert_eq!(parsed, status, "round trip of {name}");
            assert_eq!(
                status_store_hex(&parsed),
                status_store_hex(&status),
                "round-trip bytes of {name}"
            );
        }
    }

    /// The predicate matrix of every sample, pinned against the C++ oracle's
    /// `pred:` lines (digit order = `status_matrix`).
    #[test]
    fn status_predicate_matrix_matches_the_cpp_probe() {
        let expected = [
            (
                "creator_member",
                "111100011111111111111111111101111111111111111",
                0,
                false,
                0,
            ),
            (
                "creator_member_anon_rank",
                "111100011111111111111111111101111111111111111",
                0,
                true,
                5,
            ),
            (
                "administrator_full",
                "011100011111111111111111100001111111111111111",
                0,
                true,
                4,
            ),
            (
                "administrator_min",
                "000100001000101000000001000101111111111111110",
                0,
                false,
                1,
            ),
            (
                "administrator_edited",
                "011100010010000000000000000011111111111111110",
                0,
                false,
                2,
            ),
            (
                "member_until",
                "000100001000101000000001000101111111111111110",
                123456,
                false,
                4,
            ),
            (
                "member_plain",
                "000100001000101000000001000101111111111111110",
                0,
                false,
                0,
            ),
            (
                "restricted_member",
                "000101000000000000000000000001000000100000000",
                1000,
                false,
                1,
            ),
            (
                "restricted_non_member",
                "000011001000000000000000000000000000000000000",
                0,
                false,
                0,
            ),
            (
                "restricted_all_broadcast",
                "000100001000101000000001000101111111111111110",
                0,
                false,
                0,
            ),
            (
                "restricted_none_megagroup",
                "000101000000000000000000000000000000000000000",
                0,
                false,
                0,
            ),
            (
                "left",
                "000010001000101000000001000101111111111111110",
                0,
                false,
                0,
            ),
            (
                "banned_until",
                "000010100000000000000000000000000000000000000",
                2000,
                false,
                3,
            ),
            (
                "banned_default",
                "000010100000000000000000000000000000000000000",
                0,
                false,
                0,
            ),
            (
                "group_admin",
                "011100011001111101000010100011111111111111111",
                0,
                false,
                4,
            ),
            (
                "channel_admin_megagroup",
                "011100011001111100000010011111111111111111110",
                0,
                false,
                0,
            ),
            (
                "channel_admin_broadcast",
                "011100010111010000111100000001111111111111110",
                0,
                false,
                0,
            ),
        ];
        let samples = oracle_samples();
        for (name, status) in samples {
            // creator_non_member and banned_max have no oracle pred row
            let Some((_, exp_matrix, exp_until, exp_anon, exp_rank_len)) =
                expected.iter().find(|(n, ..)| *n == name)
            else {
                continue;
            };
            assert_eq!(
                status_matrix(&status),
                *exp_matrix,
                "predicate matrix of {name}"
            );
            assert_eq!(status.get_until_date(), *exp_until, "until of {name}");
            assert_eq!(status.is_anonymous(), *exp_anon, "anon of {name}");
            assert_eq!(status.get_rank().len(), *exp_rank_len, "rank len of {name}");
        }
    }

    /// A writer older than `MakeParticipantFlags64Bit` used a `uint32` flag
    /// word: version 30 with `Member | IS_MEMBER | CAN_SEND_MESSAGES` must
    /// parse as a member able to send messages, and re-store in the modern
    /// framing byte-identically to the C++ oracle.
    #[test]
    fn status_legacy_version_parse_reads_u32_flags() {
        let mut legacy = Vec::new();
        legacy.extend_from_slice(&30i32.to_le_bytes());
        let legacy_flags: u32 = (2 << 28) | (1 << 27) | (1 << 16);
        legacy.extend_from_slice(&legacy_flags.to_le_bytes());
        let mut parser = Parser::new_log_event(&legacy).unwrap();
        let parsed = DialogParticipantStatus::parse_impl(&mut parser).unwrap();
        parser.fetch_end().unwrap();
        assert_eq!(
            status_matrix(&parsed),
            "000100000000000000000000000001000000000000000"
        );
        assert_eq!(status_store_hex(&parsed), "3d0000000000012800000000");
    }

    /// The `effective:` oracle sample: a restricted member with the media
    /// rights set survives the store → parse → re-store cycle.
    #[test]
    fn status_effective_restricted_rights_match_the_cpp_probe() {
        let status = DialogParticipantStatus::restricted(
            RestrictedRights::from_bools(
                false,
                true,
                true,
                true,
                true,
                true,
                true,
                false,
                false,
                false,
                false,
                false,
                false,
                true,
                false,
                false,
                false,
                false,
                false,
                false,
                ChannelType::Megagroup,
            ),
            true,
            3000,
            ChannelType::Megagroup,
            "eff".to_owned(),
        );
        assert_eq!(
            status_store_hex(&status),
            "3d000000004000b93f000000b80b000003656666"
        );
        assert_eq!(status_round_trip(&status), status);
    }

    /// `DialogParticipantStatus()` — a forever-banned status without a rank
    /// (the C++ default-constructed convention).
    #[test]
    fn status_default_is_forever_banned() {
        let status = DialogParticipantStatus::default();
        assert_eq!(status, DialogParticipantStatus::banned(0, String::new()));
        assert!(status.is_banned());
        assert_eq!(status.get_until_date(), 0);
        assert_eq!(status.get_rank(), "");
    }

    /// `DialogParticipantStatus::update_restrictions` transitions: expired
    /// restricted members become members (non-members leave), expired
    /// banned/member become left, future and forever dates stay put.
    #[test]
    fn status_update_restrictions_transitions() {
        use crate::global::Global;
        let dir = std::env::temp_dir()
            .join(format!("td-rs-m53-status-{}", std::process::id()))
            .to_string_lossy()
            .into_owned();
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let opened = tddb::td_db::TdDb::open(tddb::td_db::Parameters {
            database_directory: dir,
            use_file_database: true,
            use_chat_info_database: true,
            ..Default::default()
        })
        .unwrap();
        let mut global = Global::new();
        global.init(opened.database);
        let now = global.unix_time();
        let future = now + 100_000;

        let mut restricted_member = DialogParticipantStatus::restricted(
            restricted_member_rights(),
            true,
            1000,
            ChannelType::Megagroup,
            "r".to_owned(),
        );
        restricted_member.update_restrictions(&global);
        assert!(restricted_member.is_member());
        assert_eq!(restricted_member.get_until_date(), 0);
        assert!(
            restricted_member.can_send_stickers(),
            "full rights after unrestriction"
        );
        assert!(restricted_member.can_send_audios());
        assert_eq!(restricted_member.get_rank(), "r");

        let mut restricted_non_member = DialogParticipantStatus::restricted(
            restricted_non_member_rights(),
            false,
            1000,
            ChannelType::Megagroup,
            String::new(),
        );
        restricted_non_member.update_restrictions(&global);
        assert!(restricted_non_member.is_left());
        assert_eq!(restricted_non_member.get_until_date(), 0);

        let mut banned = DialogParticipantStatus::banned(1000, String::new());
        banned.update_restrictions(&global);
        assert!(banned.is_left());
        assert_eq!(banned.get_until_date(), 0);

        let mut member = DialogParticipantStatus::member(1000, String::new());
        member.update_restrictions(&global);
        // the C++ flips the type to Left but keeps the IS_MEMBER bit, so
        // is_left() stays false for an expired member (a faithful quirk)
        assert!(!member.is_banned() && !member.is_restricted() && !member.is_administrator());
        assert!(member.is_member());
        assert_eq!(member.get_until_date(), 0);

        let mut not_expired = DialogParticipantStatus::banned(future, String::new());
        not_expired.update_restrictions(&global);
        assert!(not_expired.is_banned());
        assert_eq!(not_expired.get_until_date(), future);

        let mut forever = DialogParticipantStatus::banned(0, String::new());
        forever.update_restrictions(&global);
        assert!(forever.is_banned());
        assert_eq!(forever.get_until_date(), 0);

        // creator statuses are unreachable here; the C++ UNREACHABLE is a
        // debug assertion on the expired-creator path.
    }

    /// `DialogParticipantStatus::apply_restrictions`: the default-restriction
    /// matrix (boosters and bots diverge) and the administrator/banned arms.
    #[test]
    fn status_apply_restrictions_matrix() {
        let only_messages = RestrictedRights::from_bools(
            true,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            ChannelType::Megagroup,
        );

        let member = DialogParticipantStatus::member(0, String::new());
        let applied = member.apply_restrictions(only_messages, false, false);
        assert_eq!(applied.get_until_date(), 0);
        assert_eq!(applied.get_rank(), "");
        assert!(applied.is_member());
        assert!(applied.can_send_messages());
        assert!(!applied.can_send_stickers());
        assert!(!applied.can_send_audios());

        // boosters keep their full member rights
        let booster = member.apply_restrictions(only_messages, true, false);
        assert!(booster.can_send_stickers());
        assert!(booster.can_send_audios());

        // bots lose the admin-permission rights
        let bot = member.apply_restrictions(only_messages, false, true);
        assert!(bot.can_send_messages());
        assert!(!bot.can_change_info_and_settings());
        assert!(!bot.can_invite_users());
        assert!(!bot.can_pin_messages());

        // administrators gain the group-wide admin-permission rights unless bot
        let invite_everyone = RestrictedRights::from_bools(
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            true,
            false,
            false,
            false,
            false,
            false,
            ChannelType::Megagroup,
        );
        let admin = DialogParticipantStatus::administrator(admin_edited(), String::new(), false);
        let applied = admin.apply_restrictions(invite_everyone, false, false);
        assert!(applied.can_invite_users());
        let applied_bot = admin.apply_restrictions(invite_everyone, false, true);
        assert!(!applied_bot.can_invite_users());

        // banned can do nothing even if the restrictions allow it
        let banned = DialogParticipantStatus::banned(0, String::new());
        let applied = banned.apply_restrictions(only_messages, false, false);
        assert!(applied.is_banned());
        assert!(!applied.can_send_messages());
    }

    // ---- DialogParticipant (M.5.4) ----

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn dp_store_hex(participant: &DialogParticipant) -> String {
        let mut out = Vec::new();
        crate::store_parse::store_log_event_version(&mut out);
        participant.store(&mut out);
        hex(&out)
    }

    fn dp_round_trip(participant: &DialogParticipant) -> DialogParticipant {
        let mut out = Vec::new();
        crate::store_parse::store_log_event_version(&mut out);
        participant.store(&mut out);
        let mut parser = Parser::new_log_event(&out).unwrap();
        DialogParticipant::parse_impl(&mut parser).unwrap()
    }

    fn parse_legacy_bytes(bytes_hex: &str) -> DialogParticipant {
        let bytes: Vec<u8> = (0..bytes_hex.len() / 2)
            .map(|i| u8::from_str_radix(&bytes_hex[2 * i..2 * i + 2], 16).unwrap())
            .collect();
        let mut parser = Parser::new_log_event(&bytes).unwrap();
        DialogParticipant::parse_impl(&mut parser).unwrap()
    }

    #[test]
    fn dialog_id_type_table_matches_the_cpp() {
        let none = |id: i64| !DialogId::new(id).is_valid();
        let type_of = |id: i64| DialogId::new(id).get_type();

        assert!(none(0));
        assert_eq!(type_of(1), DialogType::User);
        assert_eq!(type_of(UserId::MAX_USER_ID), DialogType::User);
        assert_eq!(type_of(UserId::MAX_USER_ID + 1), DialogType::None);

        // basic groups: [-MAX_CHAT_ID, -1]
        assert_eq!(type_of(-1), DialogType::Chat);
        assert_eq!(type_of(-ChatId::MAX_CHAT_ID), DialogType::Chat);
        // ZERO_CHANNEL_ID itself is carved out of the channel band and falls
        // through to the secret-chat window (DialogId.cpp:27-39 order)
        assert_eq!(type_of(DialogId::ZERO_CHANNEL_ID), DialogType::SecretChat);
        assert_eq!(type_of(DialogId::ZERO_CHANNEL_ID - 1), DialogType::Channel);
        assert_eq!(
            type_of(DialogId::ZERO_CHANNEL_ID - ChannelId::MAX_CHANNEL_ID),
            DialogType::Channel
        );
        // the secret-chat window is [ZERO_SECRET_CHAT_ID + i32::MIN, -MAX_CHAT_ID-1)
        assert_eq!(
            type_of(DialogId::ZERO_SECRET_CHAT_ID + i32::MIN as i64),
            DialogType::SecretChat
        );
        assert_eq!(
            type_of(DialogId::ZERO_CHANNEL_ID - ChannelId::MAX_CHANNEL_ID - 1),
            DialogType::SecretChat
        );
        // ZERO_SECRET_CHAT_ID is carved out of the secret-chat window and is
        // caught by the monoforum tail — a genuine C++ quirk
        assert_eq!(type_of(DialogId::ZERO_SECRET_CHAT_ID), DialogType::Channel);
        assert_eq!(
            type_of(DialogId::ZERO_CHANNEL_ID - ChannelId::MAX_MONOFORUM_CHANNEL_ID),
            DialogType::Channel
        );
        assert_eq!(
            type_of(DialogId::ZERO_CHANNEL_ID - ChannelId::MAX_MONOFORUM_CHANNEL_ID - 1),
            DialogType::None
        );
        assert!(none(i64::MIN));
    }

    #[test]
    fn dialog_id_conversions_and_getters() {
        // invalid sources collapse to 0
        assert_eq!(DialogId::from_user_id(UserId::new(0)), DialogId::new(0));
        assert_eq!(
            DialogId::from_user_id(UserId::new(UserId::MAX_USER_ID + 1)),
            DialogId::new(0)
        );
        assert_eq!(DialogId::from_chat_id(ChatId::new(0)), DialogId::new(0));
        assert_eq!(
            DialogId::from_chat_id(ChatId::new(ChatId::MAX_CHAT_ID + 1)),
            DialogId::new(0)
        );
        assert_eq!(
            DialogId::from_channel_id(ChannelId::new(-1)),
            DialogId::new(0)
        );
        assert_eq!(
            DialogId::from_secret_chat_id(SecretChatId::new(0)),
            DialogId::new(0)
        );

        let user = DialogId::from_user_id(UserId::new(3000000002));
        assert_eq!(user.get(), 3000000002);
        assert_eq!(user.get_user_id(), UserId::new(3000000002));

        let chat = DialogId::from_chat_id(ChatId::new(999999999999));
        assert_eq!(chat.get(), -999999999999);
        assert_eq!(chat.get_chat_id(), ChatId::new(999999999999));

        let channel = DialogId::from_channel_id(ChannelId::new(1000000002));
        assert_eq!(channel.get(), -1001000000002);
        assert_eq!(channel.get_channel_id(), ChannelId::new(1000000002));

        let monoforum =
            DialogId::from_channel_id(ChannelId::new(ChannelId::MIN_MONOFORUM_CHANNEL_ID));
        assert_eq!(monoforum.get_type(), DialogType::Channel);
        assert_eq!(
            monoforum.get_channel_id(),
            ChannelId::new(ChannelId::MIN_MONOFORUM_CHANNEL_ID)
        );

        let secret = DialogId::from_secret_chat_id(SecretChatId::new(7));
        assert_eq!(secret.get(), -1999999999993);
        assert_eq!(secret.get_secret_chat_id(), SecretChatId::new(7));

        // wrong-type accessors assert
        assert!(std::panic::catch_unwind(|| user.get_chat_id()).is_err());
        assert!(std::panic::catch_unwind(|| user.get_channel_id()).is_err());
        assert!(std::panic::catch_unwind(|| chat.get_user_id()).is_err());
    }

    #[test]
    fn dialog_id_peer_conversions() {
        let peer_user = telegram_api::Peer::peerUser(telegram_api::peerUser {
            user_id: 3000000002,
        });
        assert_eq!(
            DialogId::from_peer(&peer_user),
            DialogId::from_user_id(UserId::new(3000000002))
        );

        let peer_chat = telegram_api::Peer::peerChat(telegram_api::peerChat {
            chat_id: 1234567890,
        });
        assert_eq!(
            DialogId::from_peer(&peer_chat),
            DialogId::from_chat_id(ChatId::new(1234567890))
        );

        let peer_channel = telegram_api::Peer::peerChannel(telegram_api::peerChannel {
            channel_id: 1000000002,
        });
        assert_eq!(
            DialogId::from_peer(&peer_channel),
            DialogId::from_channel_id(ChannelId::new(1000000002))
        );

        // invalid peers log and collapse to 0
        let bad_user = telegram_api::Peer::peerUser(telegram_api::peerUser { user_id: 0 });
        assert_eq!(DialogId::from_peer(&bad_user), DialogId::new(0));
        let bad_channel =
            telegram_api::Peer::peerChannel(telegram_api::peerChannel { channel_id: -5 });
        assert_eq!(DialogId::from_peer(&bad_channel), DialogId::new(0));

        let wrapped = telegram_api::DialogPeer::dialogPeer(telegram_api::dialogPeer {
            peer: Some(peer_channel),
        });
        assert_eq!(
            DialogId::from_dialog_peer(&wrapped),
            DialogId::from_channel_id(ChannelId::new(1000000002))
        );
        let folder = telegram_api::DialogPeer::dialogPeerFolder(telegram_api::dialogPeerFolder {
            folder_id: 1,
        });
        assert_eq!(DialogId::from_dialog_peer(&folder), DialogId::new(0));
        let community =
            telegram_api::DialogPeer::dialogPeerCommunity(telegram_api::dialogPeerCommunity {
                community_id: 1,
            });
        assert_eq!(DialogId::from_dialog_peer(&community), DialogId::new(0));
    }

    #[test]
    fn dialog_id_store_parse() {
        let mut out = Vec::new();
        DialogId::from_channel_id(ChannelId::new(1000000002)).store(&mut out);
        assert_eq!(hex(&out), "fe25c0ef16ffffff");
        let mut parser = Parser::new(&out);
        assert_eq!(
            DialogId::parse_impl(&mut parser).unwrap(),
            DialogId::new(-1001000000002)
        );

        for id in [
            0i64,
            1,
            -999999999999,
            -1001000000002,
            -1999999999993,
            i64::MIN,
            i64::MAX,
        ] {
            let mut out = Vec::new();
            DialogId::new(id).store(&mut out);
            let mut parser = Parser::new(&out);
            assert_eq!(
                DialogId::parse_impl(&mut parser).unwrap(),
                DialogId::new(id)
            );
        }
    }

    #[test]
    fn dialog_participant_ctor_fixups() {
        // an invalid-but-nonzero inviter collapses to the zero user
        let participant = DialogParticipant::new(
            DialogId::from_user_id(UserId::new(3000000002)),
            UserId::new(-1294967263),
            1700000000,
            DialogParticipantStatus::member(0, String::new()),
        );
        assert_eq!(participant.inviter_user_id_, UserId::new(0));
        assert_eq!(participant.joined_date_, 1700000000);

        // a negative join date collapses to 0
        let participant = DialogParticipant::new(
            DialogId::from_user_id(UserId::new(3000000002)),
            UserId::new(3000000003),
            -1,
            DialogParticipantStatus::member(0, String::new()),
        );
        assert_eq!(participant.joined_date_, 0);

        let participant = DialogParticipant::new(
            DialogId::from_user_id(UserId::new(3000000002)),
            UserId::new(3000000003),
            1700000000,
            DialogParticipantStatus::member(0, "kept".to_owned()),
        );
        assert_eq!(participant.joined_date_, 1700000000);
        assert!(participant.status_.is_member());
    }

    #[test]
    fn dialog_participant_left_and_private_member() {
        let left = DialogParticipant::left(DialogId::from_chat_id(ChatId::new(1234567890)));
        assert_eq!(left.dialog_id_, DialogId::new(-1234567890));
        assert_eq!(left.inviter_user_id_, UserId::new(0));
        assert_eq!(left.joined_date_, 0);
        assert!(left.status_.is_left());

        let member = DialogParticipant::private_member(UserId::new(4000000001), UserId::new(0));
        assert_eq!(
            member.dialog_id_,
            DialogId::from_user_id(UserId::new(4000000001))
        );
        // no valid other side: the inviter is the member themself
        assert_eq!(member.inviter_user_id_, UserId::new(4000000001));
        assert!(member.status_.is_member());

        let member =
            DialogParticipant::private_member(UserId::new(4000000001), UserId::new(4000000002));
        assert_eq!(member.inviter_user_id_, UserId::new(4000000002));
    }

    #[test]
    fn dialog_participant_is_valid_matrix() {
        // default/left: a zero dialog is invalid
        assert!(!DialogParticipant::default().is_valid());
        assert!(DialogParticipant::left(DialogId::from_chat_id(ChatId::new(1))).is_valid());

        let member = |inviter: i64, date: i32| {
            DialogParticipant::new(
                DialogId::from_user_id(UserId::new(3000000002)),
                UserId::new(inviter),
                date,
                DialogParticipantStatus::member(0, String::new()),
            )
        };
        assert!(member(0, 1700000000).is_valid());
        // the ctor normalizes a negative join date, so one only reaches
        // is_valid through the aggregate-init path — where it invalidates
        let raw = DialogParticipant {
            dialog_id_: DialogId::from_user_id(UserId::new(3000000002)),
            inviter_user_id_: UserId::new(3000000003),
            joined_date_: -1,
            status_: DialogParticipantStatus::member(0, String::new()),
        };
        assert!(!raw.is_valid());

        // restricted/banned/appointed administrators must know their inviter
        let restricted = |inviter: i64| {
            DialogParticipant::new(
                DialogId::from_user_id(UserId::new(3000000002)),
                UserId::new(inviter),
                1700000000,
                DialogParticipantStatus::restricted(
                    RestrictedRights::restrict_all(),
                    true,
                    1700003600,
                    ChannelType::Megagroup,
                    String::new(),
                ),
            )
        };
        assert!(restricted(3000000003).is_valid());
        assert!(!restricted(0).is_valid());

        let banned = |inviter: i64, until: i32| {
            DialogParticipant::new(
                DialogId::from_user_id(UserId::new(3000000002)),
                UserId::new(inviter),
                1700000000,
                DialogParticipantStatus::banned(until, String::new()),
            )
        };
        assert!(banned(3000000003, 0).is_valid());
        assert!(!banned(0, 0).is_valid());

        let admin = |inviter: i64, is_current_user_creator: bool| {
            DialogParticipant::new(
                DialogId::from_channel_id(ChannelId::new(2)),
                UserId::new(inviter),
                0,
                DialogParticipantStatus::channel_administrator(is_current_user_creator, true),
            )
        };
        assert!(!admin(0, false).is_valid());
        assert!(admin(3000000003, false).is_valid());
        // a legacy GroupAdministrator stays an administrator even when built
        // with is_current_user_creator — the inviter is still required
        assert!(!admin(0, true).is_valid());

        // a true Creator status is exempt from the inviter requirement
        let creator = DialogParticipant::new(
            DialogId::from_channel_id(ChannelId::new(2)),
            UserId::new(0),
            0,
            DialogParticipantStatus::creator(true, false, String::new()),
        );
        assert!(creator.is_valid());
    }

    #[test]
    fn dialog_participant_record_bytes_match_the_cpp() {
        let cases: Vec<(&str, DialogParticipant)> = vec![
            ("default", DialogParticipant::default()),
            (
                "left",
                DialogParticipant::left(DialogId::from_chat_id(ChatId::new(1234567890))),
            ),
            (
                "private_member",
                DialogParticipant::private_member(UserId::new(4000000001), UserId::new(0)),
            ),
            (
                "member",
                DialogParticipant::new(
                    DialogId::from_user_id(UserId::new(3000000002)),
                    UserId::new(3000000003),
                    1700000000,
                    DialogParticipantStatus::member(0, "member rank".to_owned()),
                ),
            ),
            (
                "member_until",
                DialogParticipant::new(
                    DialogId::from_channel_id(ChannelId::new(1234567890)),
                    UserId::new(3000000003),
                    1700000001,
                    DialogParticipantStatus::member(1893456000, String::new()),
                ),
            ),
            (
                "creator",
                DialogParticipant::new(
                    DialogId::from_channel_id(ChannelId::new(2)),
                    UserId::new(3000000003),
                    0,
                    DialogParticipantStatus::creator(true, true, "creator rank".to_owned()),
                ),
            ),
            (
                "restricted",
                DialogParticipant::new(
                    DialogId::from_user_id(UserId::new(3000000004)),
                    UserId::new(3000000005),
                    1700000002,
                    DialogParticipantStatus::restricted(
                        RestrictedRights::restrict_all(),
                        true,
                        1700003600,
                        ChannelType::Megagroup,
                        "restricted rank".to_owned(),
                    ),
                ),
            ),
            (
                "banned",
                DialogParticipant::new(
                    DialogId::from_user_id(UserId::new(3000000006)),
                    UserId::new(3000000007),
                    1700000003,
                    DialogParticipantStatus::banned(2147483647, String::new()),
                ),
            ),
        ];
        // conformance/dialog_participant_oracle/oracle.out `dp:` lines
        let expected: Vec<(&str, &str)> = vec![
            (
                "default",
                "3d00000000000000000000000000000000000000000000000010fd47ff010000",
            ),
            (
                "left",
                "3d0000002efd69b6ffffffff0000000000000000000000000010fd47ff010000",
            ),
            (
                "private_member",
                "3d00000001286bee0000000001286bee00000000000000000010fd2fff010000",
            ),
            (
                "member",
                "3d000000025ed0b200000000035ed0b20000000000f153650050fd2fff0100000b6d656d6265722072616e6b",
            ),
            (
                "member_until",
                "3d0000002eedc4e116ffffff035ed0b20000000001f153650010fdafff01000080d8db70",
            ),
            (
                "creator",
                "3d000000feef5a2b17ffffff035ed0b20000000000000000df7ffd0fff017f000c63726561746f722072616e6b000000",
            ),
            (
                "restricted",
                "3d000000045ed0b200000000055ed0b20000000002f15365004000b80000000010ff53650f726573747269637465642072616e6b",
            ),
            (
                "banned",
                "3d000000065ed0b200000000075ed0b20000000003f153650000005000000000",
            ),
        ];
        for ((name, participant), (expected_name, expected_hex)) in
            cases.iter().zip(expected.iter())
        {
            assert_eq!(name, expected_name);
            assert_eq!(
                &dp_store_hex(participant),
                expected_hex,
                "store mismatch for {name}"
            );
            assert_eq!(
                &dp_round_trip(participant),
                participant,
                "round-trip mismatch for {name}"
            );
        }
    }

    #[test]
    fn chat_participant_conversion_vectors_match_the_cpp() {
        let member = DialogParticipant::from_chat_participant(
            telegram_api::ChatParticipant::chatParticipant(telegram_api::chatParticipant {
                flags: 1,
                user_id: 3000000010,
                inviter_id: 3000000011,
                date: 1700001000,
                rank: "member rank".to_owned(),
            }),
            1600000000,
            false,
        );
        assert_eq!(
            member.dialog_id_,
            DialogId::from_user_id(UserId::new(3000000010))
        );
        assert_eq!(member.inviter_user_id_, UserId::new(3000000011));
        assert_eq!(member.joined_date_, 1700001000);
        assert!(member.status_.is_member());
        assert_eq!(member.status_.get_rank(), "member rank");

        let creator = DialogParticipant::from_chat_participant(
            telegram_api::ChatParticipant::chatParticipantCreator(
                telegram_api::chatParticipantCreator {
                    flags: 1,
                    user_id: 3000000012,
                    rank: "owner".to_owned(),
                },
            ),
            1600000001,
            true,
        );
        assert_eq!(
            creator.dialog_id_,
            DialogId::from_user_id(UserId::new(3000000012))
        );
        // the creator invites themself and carries the chat creation date
        assert_eq!(creator.inviter_user_id_, UserId::new(3000000012));
        assert_eq!(creator.joined_date_, 1600000001);
        assert!(creator.status_.is_creator());

        let admin = DialogParticipant::from_chat_participant(
            telegram_api::ChatParticipant::chatParticipantAdmin(
                telegram_api::chatParticipantAdmin {
                    flags: 1,
                    user_id: 3000000013,
                    inviter_id: 3000000014,
                    date: 1700001002,
                    rank: "helper".to_owned(),
                },
            ),
            1600000002,
            false,
        );
        assert_eq!(admin.inviter_user_id_, UserId::new(3000000014));
        assert_eq!(admin.joined_date_, 1700001002);
        assert!(admin.status_.is_administrator() && !admin.status_.is_creator());

        // conformance/dialog_participant_oracle/oracle.out `conv:` lines
        assert_eq!(
            dp_store_hex(&member),
            "3d0000000a5ed0b2000000000b5ed0b200000000e8f453650050fd2fff0100000b6d656d6265722072616e6b"
        );
        assert_eq!(
            dp_store_hex(&creator),
            "3d0000000c5ed0b2000000000c5ed0b20000000001105e5fdf5ffd0fff017f00056f776e65720000"
        );
        assert_eq!(
            dp_store_hex(&admin),
            "3d0000000d5ed0b2000000000e5ed0b200000000eaf45365d946fd18ff0050000668656c70657200"
        );
        assert_eq!(dp_round_trip(&member), member);
        assert_eq!(dp_round_trip(&creator), creator);
        assert_eq!(dp_round_trip(&admin), admin);
    }

    #[test]
    fn channel_participant_conversion_vectors_match_the_cpp() {
        let member = DialogParticipant::from_channel_participant(
            telegram_api::ChannelParticipant::channelParticipant(
                telegram_api::channelParticipant {
                    flags: 5,
                    user_id: 3000000015,
                    date: 1700001003,
                    subscription_until_date: 1893456001,
                    rank: "sub rank".to_owned(),
                },
            ),
            ChannelType::Megagroup,
        );
        assert_eq!(
            member.dialog_id_,
            DialogId::from_user_id(UserId::new(3000000015))
        );
        assert_eq!(member.inviter_user_id_, UserId::new(0));
        assert_eq!(member.joined_date_, 1700001003);
        assert!(member.status_.is_member());

        let self_participant = DialogParticipant::from_channel_participant(
            telegram_api::ChannelParticipant::channelParticipantSelf(
                telegram_api::channelParticipantSelf {
                    flags: 1,
                    via_request: true,
                    user_id: 3000000016,
                    inviter_id: 3000000017,
                    date: 1700001004,
                    subscription_until_date: 0,
                    rank: String::new(),
                },
            ),
            ChannelType::Broadcast,
        );
        assert_eq!(self_participant.inviter_user_id_, UserId::new(3000000017));
        assert!(self_participant.status_.is_member());

        let creator = DialogParticipant::from_channel_participant(
            telegram_api::ChannelParticipant::channelParticipantCreator(
                telegram_api::channelParticipantCreator {
                    user_id: 3000000018,
                    admin_rights: Some(telegram_api::ChatAdminRights {
                        anonymous: true,
                        change_info: true,
                        invite_users: true,
                        pin_messages: true,
                        ..Default::default()
                    }),
                    rank: "owner".to_owned(),
                    ..Default::default()
                },
            ),
            ChannelType::Megagroup,
        );
        assert_eq!(creator.joined_date_, 0);
        assert!(creator.status_.is_creator());

        let admin = DialogParticipant::from_channel_participant(
            telegram_api::ChannelParticipant::channelParticipantAdmin(
                telegram_api::channelParticipantAdmin {
                    can_edit: true,
                    user_id: 3000000019,
                    promoted_by: 3000000021,
                    date: 1700001005,
                    admin_rights: Some(telegram_api::ChatAdminRights {
                        change_info: true,
                        invite_users: true,
                        ..Default::default()
                    }),
                    rank: "moderator".to_owned(),
                    ..Default::default()
                },
            ),
            ChannelType::Megagroup,
        );
        assert_eq!(admin.inviter_user_id_, UserId::new(3000000021));
        assert_eq!(admin.joined_date_, 1700001005);
        assert!(admin.status_.is_administrator() && !admin.status_.is_creator());

        let left = DialogParticipant::from_channel_participant(
            telegram_api::ChannelParticipant::channelParticipantLeft(
                telegram_api::channelParticipantLeft {
                    peer: Some(telegram_api::Peer::peerChannel(telegram_api::peerChannel {
                        channel_id: 1000000002,
                    })),
                },
            ),
            ChannelType::Megagroup,
        );
        assert_eq!(
            left.dialog_id_,
            DialogId::from_channel_id(ChannelId::new(1000000002))
        );
        assert!(left.status_.is_left());

        let banned = DialogParticipant::from_channel_participant(
            telegram_api::ChannelParticipant::channelParticipantBanned(
                telegram_api::channelParticipantBanned {
                    flags: 5,
                    left: false,
                    peer: Some(telegram_api::Peer::peerUser(telegram_api::peerUser {
                        user_id: 3000000022,
                    })),
                    kicked_by: 3000000023,
                    date: 1700001006,
                    banned_rights: Some(telegram_api::ChatBannedRights {
                        send_messages: true,
                        until_date: 1700003600,
                        ..Default::default()
                    }),
                    rank: "spam".to_owned(),
                },
            ),
            ChannelType::Megagroup,
        );
        assert_eq!(
            banned.dialog_id_,
            DialogId::from_user_id(UserId::new(3000000022))
        );
        assert_eq!(banned.inviter_user_id_, UserId::new(3000000023));
        assert!(banned.status_.is_restricted() && banned.status_.is_member());

        // view_messages turns the rights into a plain banned; `left` + broadcast
        // degrade to Left()
        let banned_view = DialogParticipant::from_channel_participant(
            telegram_api::ChannelParticipant::channelParticipantBanned(
                telegram_api::channelParticipantBanned {
                    flags: 1,
                    left: true,
                    peer: Some(telegram_api::Peer::peerChat(telegram_api::peerChat {
                        chat_id: 1234567890,
                    })),
                    kicked_by: 0,
                    date: 1700001007,
                    banned_rights: Some(telegram_api::ChatBannedRights {
                        view_messages: true,
                        ..Default::default()
                    }),
                    rank: String::new(),
                },
            ),
            ChannelType::Broadcast,
        );
        assert_eq!(
            banned_view.dialog_id_,
            DialogId::from_chat_id(ChatId::new(1234567890))
        );
        assert_eq!(banned_view.joined_date_, 1700001007);
        assert!(banned_view.status_.is_banned());

        // conformance/dialog_participant_oracle/oracle.out `conv:` lines
        assert_eq!(
            dp_store_hex(&member),
            "3d0000000f5ed0b2000000000000000000000000ebf453650050fdafff01000081d8db70087375622072616e6b000000"
        );
        assert_eq!(
            dp_store_hex(&self_participant),
            "3d000000105ed0b200000000115ed0b200000000ecf453650010fd2fff010000"
        );
        assert_eq!(
            dp_store_hex(&creator),
            "3d000000125ed0b200000000000000000000000000000000df7ffd0fff017f00056f776e65720000"
        );
        assert_eq!(
            dp_store_hex(&admin),
            "3d000000135ed0b200000000155ed0b200000000edf4536511c4fd18ff000000096d6f64657261746f720000"
        );
        assert_eq!(
            dp_store_hex(&left),
            "3d000000fe25c0ef16ffffff0000000000000000000000000010fd47ff010000"
        );
        assert_eq!(
            dp_store_hex(&banned),
            "3d000000165ed0b200000000175ed0b200000000eef453650050fdbfff01000010ff5365047370616d000000"
        );
        assert_eq!(
            dp_store_hex(&banned_view),
            "3d0000002efd69b6ffffffff0000000000000000eff453650000005000000000"
        );
        assert_eq!(dp_round_trip(&member), member);
        assert_eq!(dp_round_trip(&banned), banned);
        assert_eq!(dp_round_trip(&banned_view), banned_view);
    }

    #[test]
    fn dialog_participant_legacy_parse_vectors() {
        // v30 (pre-SupportBannedChannels): the dialog is a bare int32 user id,
        // the inviter an int32, the status flag word a uint32. The status tail
        // is the uint32 truncation of the modern Member(0, "") word.
        let participant = parse_legacy_bytes("1e00000001cf8d7f02cf8d7fd0f853650010fd2f");
        assert_eq!(
            participant.dialog_id_,
            DialogId::from_user_id(UserId::new(2140000001))
        );
        assert_eq!(participant.inviter_user_id_, UserId::new(2140000002));
        assert_eq!(participant.joined_date_, 1700002000);
        // re-stored at version 61 (oracle `rt:` line)
        assert_eq!(
            dp_store_hex(&participant),
            "3d00000001cf8d7f0000000002cf8d7f00000000d0f853650010fd2f00000000"
        );

        // v31: the dialog grows to a long, but the inviter is still an int32
        // and wraps negative — the C++ keeps the garbage inviter as-is.
        let participant = parse_legacy_bytes("1f00000002baf56617ffffff215ed0b2d1f853650010fd2f");
        assert_eq!(participant.dialog_id_, DialogId::new(-998999999998));
        assert_eq!(participant.inviter_user_id_, UserId::new(-1294967263));
        assert_eq!(participant.joined_date_, 1700002001);
        assert_eq!(
            dp_store_hex(&participant),
            "3d00000002baf56617ffffff215ed0b2ffffffffd1f853650010fd2f00000000"
        );

        // v33: the inviter reads as a long.
        let participant =
            parse_legacy_bytes("2100000002baf56617ffffff235ed0b200000000d2f853650010fd2f");
        assert_eq!(participant.inviter_user_id_, UserId::new(3000000035));
        assert_eq!(participant.joined_date_, 1700002002);
        assert_eq!(
            dp_store_hex(&participant),
            "3d00000002baf56617ffffff235ed0b200000000d2f853650010fd2f00000000"
        );
    }

    // ---- DialogAdministrator / BotCommand / BotCommands / MinChannel (M.5.5) ----

    #[test]
    fn dialog_administrator_record_matches_the_cpp() {
        let admin =
            DialogAdministrator::new(UserId::new(3000000003), "boss".to_owned(), true, true);
        assert_eq!(admin.get_user_id(), UserId::new(3000000003));
        assert_eq!(admin.get_rank(), "boss");
        assert!(admin.is_creator());
        assert!(admin.can_be_edited());

        // the byte format: the flag block, the user id, the optional rank
        let mut out = Vec::new();
        admin.store(&mut out);
        assert_eq!(hex(&out), "07000000035ed0b20000000004626f7373000000");
        let mut parser = Parser::new(&out);
        assert_eq!(DialogAdministrator::parse_impl(&mut parser).unwrap(), admin);

        // set_rank reports the change like the C++
        let mut renamed = admin.clone();
        assert!(renamed.set_rank("chief"));
        assert!(!renamed.set_rank("chief"));
        assert_eq!(renamed.get_rank(), "chief");

        // a rankless administrator stores no rank
        let rankless = DialogAdministrator::new(UserId::new(5), String::new(), false, false);
        let mut out = Vec::new();
        rankless.store(&mut out);
        assert_eq!(hex(&out), "000000000500000000000000");
        let mut parser = Parser::new(&out);
        assert_eq!(
            DialogAdministrator::parse_impl(&mut parser).unwrap(),
            rankless
        );

        // from_status narrows to administrators
        let status = DialogParticipantStatus::channel_administrator(true, true);
        let admin = DialogAdministrator::from_status(UserId::new(3000000004), &status);
        assert_eq!(admin.get_rank(), "");
        assert!(!admin.is_creator());
        assert!(admin.can_be_edited());
        assert!(
            std::panic::catch_unwind(|| {
                DialogAdministrator::from_status(UserId::new(1), &DialogParticipantStatus::left())
            })
            .is_err()
        );
    }

    #[test]
    fn bot_command_record_matches_the_cpp() {
        let command = BotCommand::from_telegram(telegram_api::BotCommand {
            ephemeral: true,
            command: "/start@bot".to_owned(),
            description: "Start".to_owned(),
            ..Default::default()
        });
        assert!(command.is_ephemeral());

        // the flag block, then the two TL strings
        let mut out = Vec::new();
        command.store(&mut out);
        assert_eq!(
            hex(&out),
            "010000000a2f737461727440626f74000553746172740000"
        );
        let mut parser = Parser::new(&out);
        assert_eq!(BotCommand::parse_impl(&mut parser).unwrap(), command);

        // pre-SupportEphemeralMessages blobs carry no flag block
        let mut legacy = Vec::new();
        store_i32(&mut legacy, 60);
        store_string(&mut legacy, "/old");
        store_string(&mut legacy, "Old");
        let mut parser = Parser::new_log_event(&legacy).unwrap();
        let old = BotCommand::parse_impl(&mut parser).unwrap();
        assert!(!old.is_ephemeral());
        assert_eq!(old.command_, "/old");

        let plain = BotCommand::new("/a".to_owned(), "b".to_owned(), false);
        let mut out = Vec::new();
        plain.store(&mut out);
        assert_eq!(hex(&out), "00000000022f610001620000");
    }

    #[test]
    fn bot_commands_update_all_matches_the_cpp() {
        let mut all = Vec::new();
        let start = BotCommands::from_telegram(
            UserId::new(3000000003),
            vec![telegram_api::BotCommand {
                ephemeral: false,
                command: "/start".to_owned(),
                description: "Start".to_owned(),
                ..Default::default()
            }],
        );
        // a new bot appends and reports the change
        assert!(BotCommands::update_all_bot_commands(&mut all, start));
        assert_eq!(all.len(), 1);

        // the same commands again change nothing
        let same = BotCommands::from_telegram(
            UserId::new(3000000003),
            vec![telegram_api::BotCommand {
                ephemeral: false,
                command: "/start".to_owned(),
                description: "Start".to_owned(),
                ..Default::default()
            }],
        );
        assert!(!BotCommands::update_all_bot_commands(&mut all, same));

        // a changed list replaces in place
        let changed = BotCommands::from_telegram(
            UserId::new(3000000003),
            vec![telegram_api::BotCommand {
                ephemeral: false,
                command: "/help".to_owned(),
                description: "Help".to_owned(),
                ..Default::default()
            }],
        );
        assert!(BotCommands::update_all_bot_commands(&mut all, changed));
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].commands_.len(), 1);
        assert_eq!(all[0].commands_[0].command_, "/help");

        // a second bot joins the list
        let second_add = BotCommands::from_telegram(
            UserId::new(3000000004),
            vec![telegram_api::BotCommand {
                ephemeral: false,
                command: "/ping".to_owned(),
                description: "Ping".to_owned(),
                ..Default::default()
            }],
        );
        assert!(BotCommands::update_all_bot_commands(&mut all, second_add));
        assert_eq!(all.len(), 2);

        // an empty list drops the bot's entry; dropping twice is a no-op
        let drop_second = BotCommands::from_telegram(UserId::new(3000000004), Vec::new());
        assert!(BotCommands::update_all_bot_commands(&mut all, drop_second));
        assert_eq!(all.len(), 1);
        let nothing = BotCommands::from_telegram(UserId::new(3000000004), Vec::new());
        assert!(!BotCommands::update_all_bot_commands(&mut all, nothing));
    }

    #[test]
    fn bot_commands_store_parse_round_trips() {
        let commands = BotCommands::from_telegram(
            UserId::new(3000000003),
            vec![
                telegram_api::BotCommand {
                    ephemeral: false,
                    command: "/start".to_owned(),
                    description: "Start".to_owned(),
                    ..Default::default()
                },
                telegram_api::BotCommand {
                    ephemeral: true,
                    command: "/now".to_owned(),
                    description: String::new(),
                    ..Default::default()
                },
            ],
        );
        let mut out = Vec::new();
        crate::store_parse::store_log_event_version(&mut out);
        commands.store(&mut out);
        let mut parser = Parser::new_log_event(&out).unwrap();
        assert_eq!(BotCommands::parse_impl(&mut parser).unwrap(), commands);
        parser.fetch_end().unwrap();

        // pre-Support64BitIds blobs carry the bot user id as an int32 and no
        // per-command flag block
        let mut legacy = Vec::new();
        store_i32(&mut legacy, 32);
        store_i32(&mut legacy, 2140000001);
        store_i32(&mut legacy, 1); // one command
        store_string(&mut legacy, "/old");
        store_string(&mut legacy, "Old");
        let mut parser = Parser::new_log_event(&legacy).unwrap();
        let old = BotCommands::parse_impl(&mut parser).unwrap();
        assert_eq!(old.get_bot_user_id(), UserId::new(2140000001));
        assert_eq!(old.commands_.len(), 1);
        assert!(!old.commands_[0].is_ephemeral());
    }

    #[test]
    fn min_channel_defaults_like_the_cpp() {
        let channel = MinChannel::default();
        assert_eq!(channel.title_, "");
        assert_eq!(channel.photo_, DialogPhoto::default());
        assert_eq!(channel.accent_color_id_, AccentColorId::default());
        assert!(!channel.is_megagroup_);

        let megagroup = MinChannel {
            title_: "chat".to_owned(),
            is_megagroup_: true,
            ..Default::default()
        };
        assert!(megagroup.is_megagroup_);
    }

    // ---- StarSubscriptionPricing / DialogInviteLink / MissingInvitee /
    // ---- VerificationStatus (M.5.6, M.5.7) ----

    /// A minimal `Global` + `OptionManager` pair for the object-builder
    /// tests; `get_user_id_object` touches neither yet.
    struct BareEnv {
        global: Global,
        options: crate::option_manager::OptionManager,
    }

    impl BareEnv {
        fn new() -> BareEnv {
            BareEnv {
                global: Global::new(),
                options: crate::option_manager::OptionManager::new(false),
            }
        }

        fn env(&mut self) -> UserEnv<'_> {
            UserEnv {
                global: &mut self.global,
                options: &mut self.options,
                auth_is_bot: false,
            }
        }
    }

    fn exported_invite() -> telegram_api::chatInviteExported {
        telegram_api::chatInviteExported {
            flags: 0,
            revoked: false,
            permanent: false,
            request_needed: false,
            link: "https://t.me/+abc".to_owned(),
            admin_id: 3000000005,
            date: 1_600_000_000,
            start_date: 0,
            expire_date: 0,
            usage_limit: 0,
            usage: 0,
            requested: 0,
            subscription_expired: 0,
            title: String::new(),
            subscription_pricing: Some(telegram_api::StarsSubscriptionPricing::default()),
        }
    }

    #[test]
    fn star_subscription_pricing_matches_the_cpp() {
        // the telegram form sanitizes through get_star_count
        let pricing =
            StarSubscriptionPricing::from_telegram(Some(telegram_api::StarsSubscriptionPricing {
                period: 30,
                amount: 100,
            }));
        assert_eq!(pricing.get_period(), 30);
        assert_eq!(pricing.get_amount(), 100);
        assert!(!pricing.is_empty());
        assert_eq!(
            pricing.get_star_subscription_pricing_object().unwrap(),
            td_api::StarSubscriptionPricing {
                period: 30,
                star_count: 100
            }
        );
        assert_eq!(
            pricing.get_input_stars_subscription_pricing().unwrap(),
            telegram_api::StarsSubscriptionPricing {
                period: 30,
                amount: 100
            }
        );

        // a null pointer and the clamps
        assert!(StarSubscriptionPricing::from_telegram(None).is_empty());
        assert_eq!(get_star_count(-5, false), 0);
        assert_eq!(get_star_count(-5, true), -5);
        assert_eq!(get_star_count(i64::MIN, true), -(1i64 << 51));
        assert_eq!(get_star_count((1i64 << 51) + 1, true), 1i64 << 51);
        assert_eq!(get_star_count(50, false), 50);

        // the td_api form drops amounts above a billion
        let pricing = StarSubscriptionPricing::from_td_api(Some(td_api::StarSubscriptionPricing {
            period: 7,
            star_count: 2_000_000_000,
        }));
        assert!(pricing.is_empty());
        let pricing = StarSubscriptionPricing::from_td_api(Some(td_api::StarSubscriptionPricing {
            period: 7,
            star_count: 12,
        }));
        assert_eq!(pricing.get_amount(), 12);

        // the byte format: the empty flag block, the period, the amount
        let mut out = Vec::new();
        pricing.store(&mut out);
        assert_eq!(hex(&out), "00000000070000000c00000000000000");
        let mut parser = Parser::new(&out);
        assert_eq!(
            StarSubscriptionPricing::parse_impl(&mut parser).unwrap(),
            pricing
        );
        parser.fetch_end().unwrap();
    }

    #[test]
    fn dialog_invite_link_ctor_sanitizes_like_the_cpp() {
        // a null pointer and the join-request form both yield the default
        assert_eq!(
            DialogInviteLink::from_telegram(None, false, false, "test"),
            DialogInviteLink::default()
        );
        assert_eq!(
            DialogInviteLink::from_telegram(
                Some(
                    telegram_api::ExportedChatInvite::chatInvitePublicJoinRequests(
                        telegram_api::chatInvitePublicJoinRequests
                    )
                ),
                false,
                true,
                "test"
            ),
            DialogInviteLink::default()
        );
        assert!(!DialogInviteLink::default().is_valid());

        // a well-formed link parses field by field
        let valid = DialogInviteLink::from_telegram(
            Some(telegram_api::ExportedChatInvite::chatInviteExported(
                exported_invite(),
            )),
            false,
            false,
            "test",
        );
        assert!(valid.is_valid());
        assert_eq!(valid.get_invite_link(), "https://t.me/+abc");
        assert_eq!(valid.get_creator_user_id(), UserId::new(3000000005));
        assert!(!valid.is_permanent());

        // malformed fields clamp exactly like the C++ repair path
        let malformed = DialogInviteLink::from_telegram(
            Some(telegram_api::ExportedChatInvite::chatInviteExported(
                telegram_api::chatInviteExported {
                    admin_id: 0,
                    date: 5,
                    usage_limit: -10,
                    usage: -3,
                    requested: -2,
                    ..exported_invite()
                },
            )),
            false,
            false,
            "test",
        );
        assert_eq!(malformed.get_creator_user_id(), UserId::new(0));
        assert!(!malformed.is_valid());
        // reconstructed through the private fields
        assert_eq!(malformed.date_, 0);
        assert_eq!(malformed.usage_limit_, 0);
        assert_eq!(malformed.usage_count_, 0);
        assert_eq!(malformed.request_count_, 0);

        // a permanent link may carry no extra parameters — except a
        // pending join request count (fb6203c77: primary links can create
        // join requests, so request_count survives the wipe)
        let wrong_permanent = DialogInviteLink::from_telegram(
            Some(telegram_api::ExportedChatInvite::chatInviteExported(
                telegram_api::chatInviteExported {
                    permanent: true,
                    title: "t".to_owned(),
                    expire_date: 2_000_000_000,
                    usage_limit: 10,
                    start_date: 2_000_000_000,
                    requested: 3,
                    ..exported_invite()
                },
            )),
            false,
            false,
            "test",
        );
        assert!(wrong_permanent.is_permanent());
        assert_eq!(wrong_permanent.title_, "");
        assert_eq!(wrong_permanent.expire_date_, 0);
        assert_eq!(wrong_permanent.usage_limit_, 0);
        assert_eq!(wrong_permanent.edit_date_, 0);
        assert_eq!(wrong_permanent.request_count_, 3);

        // join requests cannot coexist with a usage limit
        let join_request = DialogInviteLink::from_telegram(
            Some(telegram_api::ExportedChatInvite::chatInviteExported(
                telegram_api::chatInviteExported {
                    request_needed: true,
                    usage_limit: 10,
                    ..exported_invite()
                },
            )),
            false,
            false,
            "test",
        );
        assert!(join_request.creates_join_request_);
        assert_eq!(join_request.usage_limit_, 0);
    }

    #[test]
    fn dialog_invite_link_store_parse_matches_the_cpp() {
        let invite_link = DialogInviteLink {
            invite_link_: "https://t.me/+aB3".to_owned(),
            title_: "X".to_owned(),
            creator_user_id_: UserId::new(3000000004),
            pricing_: StarSubscriptionPricing::default(),
            date_: 1_000_000_000,
            edit_date_: 0,
            expire_date_: 2_000_000_000,
            usage_limit_: 10,
            usage_count_: 5,
            expired_usage_count_: 2,
            request_count_: 0,
            creates_join_request_: false,
            is_revoked_: false,
            is_permanent_: false,
        };

        // flags (bits 2, 3, 4, 8, 10), the link, the creator, the date, the
        // present optionals in the C++ order
        let mut out = Vec::new();
        invite_link.store(&mut out);
        assert_eq!(
            hex(&out),
            "1c050000\
             1168747470733a2f2f742e6d652f2b6142330000\
             045ed0b200000000\
             00ca9a3b\
             00943577\
             0a000000\
             05000000\
             01580000\
             02000000"
        );
        let mut parser = Parser::new(&out);
        assert_eq!(
            DialogInviteLink::parse_impl(&mut parser).unwrap(),
            invite_link
        );
        parser.fetch_end().unwrap();

        // the creates_join_request reset survives the round trip
        let with_requests = DialogInviteLink {
            creates_join_request_: true,
            usage_limit_: 0,
            ..invite_link.clone()
        };
        let mut out = Vec::new();
        with_requests.store(&mut out);
        let mut parser = Parser::new(&out);
        assert_eq!(
            DialogInviteLink::parse_impl(&mut parser).unwrap(),
            with_requests
        );

        // a default link stores the bare flag word, the empty link, the
        // creator and a zero date
        let mut out = Vec::new();
        DialogInviteLink::default().store(&mut out);
        assert_eq!(hex(&out), "0000000000000000000000000000000000000000");
    }

    #[test]
    fn dialog_invite_link_builder_matches_the_cpp() {
        let mut bare = BareEnv::new();
        let mut user_manager = UserManager::new();

        // an invalid link builds no object
        assert_eq!(
            DialogInviteLink::default()
                .get_chat_invite_link_object(&mut user_manager, &mut bare.env()),
            None
        );

        let invite_link = DialogInviteLink::from_telegram(
            Some(telegram_api::ExportedChatInvite::chatInviteExported(
                telegram_api::chatInviteExported {
                    permanent: true,
                    title: "Channel".to_owned(),
                    usage: 12,
                    requested: 4,
                    subscription_pricing: Some(telegram_api::StarsSubscriptionPricing {
                        period: 6,
                        amount: 50,
                    }),
                    expire_date: 0,
                    ..exported_invite()
                },
            )),
            false,
            false,
            "test",
        );
        // the permanent sanitizer clears the title but not the counts
        let object = invite_link
            .get_chat_invite_link_object(&mut user_manager, &mut bare.env())
            .unwrap();
        assert_eq!(object.invite_link, "https://t.me/+abc");
        assert_eq!(object.name, "");
        assert_eq!(object.creator_user_id, 3000000005);
        assert_eq!(object.date, 1_600_000_000);
        assert_eq!(object.edit_date, 0);
        assert_eq!(object.expiration_date, 0);
        assert_eq!(object.subscription_pricing.as_ref().unwrap().period, 6);
        assert_eq!(object.subscription_pricing.as_ref().unwrap().star_count, 50);
        assert_eq!(object.member_limit, 0);
        assert_eq!(object.member_count, 12);
        assert_eq!(object.expired_member_count, 0);
        // fb6203c77: the permanent sanitizer no longer clears the pending
        // join request count — primary links can create join requests
        assert_eq!(object.pending_join_request_count, 4);
        assert!(!object.creates_join_request);
        assert!(object.is_primary);
        assert!(!object.is_revoked);
    }

    #[test]
    fn missing_invitees_match_the_cpp() {
        let invitee = MissingInvitee::from_telegram(telegram_api::MissingInvitee {
            flags: 0,
            premium_would_allow_invite: true,
            premium_required_for_pm: false,
            user_id: 3000000006,
        });
        assert!(invitee.is_valid());
        assert_eq!(invitee.get_user_id(), UserId::new(3000000006));
        assert!(
            !MissingInvitee::from_telegram(telegram_api::MissingInvitee {
                user_id: 0,
                ..telegram_api::MissingInvitee::default()
            })
            .is_valid()
        );

        // invalid entries are dropped from the list form
        let invitees = MissingInvitees::from_telegram(vec![
            telegram_api::MissingInvitee {
                user_id: 0,
                ..telegram_api::MissingInvitee::default()
            },
            telegram_api::MissingInvitee {
                user_id: 42,
                premium_required_for_pm: true,
                ..telegram_api::MissingInvitee::default()
            },
        ]);
        assert_eq!(invitees.missing_invitees_.len(), 1);

        let mut bare = BareEnv::new();
        let mut user_manager = UserManager::new();
        let mut env = bare.env();
        let member = invitee.get_failed_to_add_member_object(&mut user_manager, &mut env);
        assert_eq!(member.user_id, 3000000006);
        assert!(member.premium_would_allow_invite);
        assert!(!member.premium_required_to_send_messages);
        let members = invitees.get_failed_to_add_members_object(&mut user_manager, &mut env);
        assert_eq!(members.failed_to_add_members.len(), 1);
        assert_eq!(members.failed_to_add_members[0].user_id, 42);
        assert!(members.failed_to_add_members[0].premium_required_to_send_messages);
    }

    #[test]
    fn verification_status_matches_the_cpp() {
        assert!(
            get_verification_status_object(false, false, false, CustomEmojiId::new(0)).is_none()
        );
        let status =
            get_verification_status_object(false, true, false, CustomEmojiId::new(0)).unwrap();
        assert!(!status.is_verified);
        assert!(status.is_scam);
        assert!(!status.is_fake);
        assert_eq!(status.bot_verification_icon_custom_emoji_id, 0);
        let status =
            get_verification_status_object(false, false, false, CustomEmojiId::new(531)).unwrap();
        assert_eq!(status.bot_verification_icon_custom_emoji_id, 531);
        assert!(
            get_verification_status_object(true, false, false, CustomEmojiId::new(0))
                .unwrap()
                .is_verified
        );
    }

    fn test_time_ctx() -> TimeCtx {
        TimeCtx {
            now: 1_000.0,
            server_time: 2_000.0,
        }
    }

    fn round_trip<T, S, P>(store: S, parse: P) -> T
    where
        S: FnOnce(&mut Vec<u8>),
        P: FnOnce(&[u8]) -> T,
    {
        let mut out = Vec::new();
        crate::store_parse::store_log_event_version(&mut out);
        store(&mut out);
        parse(&out)
    }

    #[test]
    fn chat_record_round_trips_and_stays_byte_stable() {
        let mut chat = Chat::new();
        chat.title = "group".to_owned();
        chat.participant_count = 7;
        chat.date = 1_600_000_000;
        chat.version = 3;
        chat.default_permissions_version = 4;
        chat.pinned_message_version = 5;
        chat.cache_version = Chat::CACHE_VERSION;
        chat.migrated_to_channel_id = ChannelId::new(1_000_000_000_003);
        chat.status = DialogParticipantStatus::creator(true, false, "boss".to_owned());
        chat.is_active = true;
        chat.noforwards = true;

        let parsed = round_trip(
            |out| chat.store(out),
            |input| Chat::parse_value(input).unwrap(),
        );
        assert_eq!(parsed, chat);
        // byte-stable: re-storing the parsed record gives the same bytes
        let mut first = Vec::new();
        chat.store(&mut first);
        let mut second = Vec::new();
        parsed.store(&mut second);
        assert_eq!(hex(&first), hex(&second));

        // the administrator-rank downgrade the C++ applies on parse
        chat.status = DialogParticipantStatus::group_administrator(false, "rank".to_owned());
        let parsed = round_trip(
            |out| chat.store(out),
            |input| Chat::parse_value(input).unwrap(),
        );
        assert!(parsed.status.is_administrator());
        assert_eq!(parsed.status.get_rank(), "");
    }

    #[test]
    fn chat_legacy_parse_reconstructs_the_old_rights() {
        // a pre-new-rights blob: version 32 keeps the id parse on int32
        let mut out = Vec::new();
        store_i32(&mut out, 32);
        store_flags(
            &mut out,
            [
                false, // left
                true,  // kicked
                false, // is_creator
                false, // is_administrator
                false, // everyone_is_administrator
                false, // can_edit
                true,  // is_active
                false, // has_photo
                false, // use_new_rights
                false, // has_default_permissions_version
                false, // has_pinned_message_version
                false, // has_cache_version
                false, // noforwards
            ],
        );
        store_string(&mut out, "legacy");
        store_i32(&mut out, 2); // participant_count
        store_i32(&mut out, 1_600_000_000); // date
        store_i32(&mut out, 1_000_000_001); // migrated_to_channel_id, int32
        store_i32(&mut out, 1); // version
        let mut parser = Parser::new_log_event(&out).unwrap();
        let chat = Chat::parse_impl(&mut parser).unwrap();
        parser.fetch_end().unwrap();

        assert!(chat.status.is_banned());
        let expected = RestrictedRights::from_bools(
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            false,
            false,
            false,
            false,
            false,
            true,
            true,
            ChannelType::Unknown,
        );
        assert_eq!(chat.default_permissions, expected);
        assert_eq!(chat.migrated_to_channel_id, ChannelId::new(1_000_000_001));
    }

    #[test]
    fn chat_full_record_round_trips() {
        let mut chat_full = ChatFull::new();
        chat_full.version = 9;
        chat_full.creator_user_id = UserId::new(3000000005);
        chat_full.participants = vec![DialogParticipant::new(
            DialogId::from_user_id(UserId::new(3000000005)),
            UserId::new(3000000006),
            1_600_000_000,
            DialogParticipantStatus::member(0, String::new()),
        )];
        chat_full.description = "about".to_owned();
        chat_full.can_set_username = true;
        chat_full.invite_link = DialogInviteLink::from_telegram(
            Some(telegram_api::ExportedChatInvite::chatInviteExported(
                exported_invite(),
            )),
            false,
            false,
            "test",
        );
        chat_full.bot_commands = vec![BotCommands::from_telegram(
            UserId::new(3000000007),
            vec![telegram_api::BotCommand {
                command: "/start".to_owned(),
                description: "Start".to_owned(),
                ..telegram_api::BotCommand::default()
            }],
        )];

        let parsed = round_trip(
            |out| chat_full.store(out),
            |input| ChatFull::parse_value(input).unwrap(),
        );
        assert_eq!(parsed, chat_full);

        // a C++-written blob with a photo is rejected, not mis-parsed
        let mut out = Vec::new();
        crate::store_parse::store_log_event_version(&mut out);
        store_flags(
            &mut out,
            [
                false, // has_description
                false, // legacy_has_invite_link
                false, // can_set_username
                true,  // has_photo
                false, // has_invite_link
                false, // has_bot_commands
            ],
        );
        assert!(ChatFull::parse_value(&out).is_err());
    }

    #[test]
    fn channel_record_round_trips() {
        let ctx = test_time_ctx();
        let mut channel = Channel::new();
        channel.access_hash = -1234567890123456789;
        channel.title = "supergroup".to_owned();
        channel.is_megagroup = true;
        channel.is_forum = true;
        channel.emoji_status = Some(EmojiStatus::default());
        channel.accent_color_id = AccentColorId::new(5);
        channel.background_custom_emoji_id = CustomEmojiId::new(42);
        channel.usernames = Usernames::from_telegram("dupe".to_owned(), Vec::new());
        channel.restriction_reasons = vec![RestrictionReason::new(
            "all".to_owned(),
            "foo".to_owned(),
            "bar".to_owned(),
        )];
        channel.status = DialogParticipantStatus::creator(true, false, String::new());
        channel.default_permissions = RestrictedRights::from_bools(
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            false,
            false,
            false,
            false,
            false,
            true,
            true,
            ChannelType::Megagroup,
        );
        channel.date = 1_600_000_000;
        channel.participant_count = 11;
        channel.boost_level = 2;
        channel.paid_message_star_count = 5;
        channel.bot_verification_icon = CustomEmojiId::new(7);
        channel.max_active_story_id = StoryId::new(30);
        channel.max_read_story_id = StoryId::new(28);
        channel.max_active_story_id_next_reload_time = ctx.now + 100.0;
        channel.monoforum_channel_id = ChannelId::new(1_002_147_483_650);
        channel.linked_community_id = CommunityId::new(55);
        channel.cache_version = Channel::CACHE_VERSION;
        channel.noforwards = true;
        channel.sign_messages = true;

        let parsed = round_trip(
            |out| channel.store(out, &ctx),
            |input| Channel::parse_value(input, &ctx).unwrap(),
        );
        // the parse forces show_message_sender for megagroups — the same
        // in-place mutation the C++ performs
        let mut expected = channel.clone();
        expected.show_message_sender = true;
        assert_eq!(parsed, expected);

        let mut first = Vec::new();
        expected.store(&mut first, &ctx);
        let mut second = Vec::new();
        parsed.store(&mut second, &ctx);
        assert_eq!(hex(&first), hex(&second));

        // megagroups force show_message_sender on parse
        assert!(parsed.show_message_sender);
    }

    #[test]
    fn channel_legacy_parse_reconstructs_the_old_statuses() {
        let ctx = test_time_ctx();
        let legacy_flags = |left: bool, kicked: bool, is_creator: bool, can_edit: bool| {
            let mut out = Vec::new();
            store_i32(&mut out, 40);
            store_flags(
                &mut out,
                [
                    left,       // left
                    kicked,     // kicked
                    false,      // anyone_can_invite
                    false,      // sign_messages
                    is_creator, // is_creator
                    can_edit,   // can_edit
                    false,      // can_moderate
                    false,      // is_megagroup
                    false,      // is_verified
                    false,      // has_photo
                    false,      // legacy_has_username
                    false,      // legacy_is_restricted
                    false,      // use_new_rights
                    false,      // has_participant_count
                    false,      // have_default_permissions
                    false,      // is_scam
                    false,      // has_cache_version
                    false,      // has_linked_channel
                    false,      // has_location
                    false,      // is_slow_mode_enabled
                    false,      // has_restriction_reasons
                    false,      // legacy_has_active_group_call
                    false,      // is_fake
                    false,      // is_gigagroup
                    false,      // noforwards
                    false,      // can_be_deleted
                    false,      // join_to_send
                    false,      // join_request
                    false,      // has_usernames
                    false,      // has_flags2
                ],
            );
            store_i64(&mut out, 0); // access_hash
            store_string(&mut out, "legacy");
            store_i32(&mut out, 1_600_000_000); // date
            out
        };
        let parse_legacy = |left: bool, kicked: bool, is_creator: bool, can_edit: bool| {
            let out = legacy_flags(left, kicked, is_creator, can_edit);
            let mut parser = Parser::new_log_event(&out).unwrap();
            Channel::parse_impl(&mut parser, &ctx).unwrap()
        };

        // kicked wins over left
        let channel = parse_legacy(true, true, false, false);
        assert!(channel.status.is_banned());

        // creator reconstruction
        let channel = parse_legacy(false, false, true, false);
        assert!(channel.status.is_creator());

        // can_edit maps to the legacy channel administrator
        let channel = parse_legacy(false, false, false, true);
        assert!(channel.status.is_administrator());

        // plain membership
        let channel = parse_legacy(false, false, false, false);
        assert!(channel.status.is_member());
    }

    #[test]
    fn channel_full_record_round_trips_and_repairs() {
        let ctx = test_time_ctx();
        let mut channel_full = ChannelFull::new();
        channel_full.description = "desc".to_owned();
        channel_full.participant_count = 100;
        channel_full.administrator_count = 3;
        channel_full.restricted_count = 1;
        channel_full.banned_count = 2;
        channel_full.boost_count = 4;
        channel_full.unrestrict_boost_count = 6;
        channel_full.gift_count = 8;
        channel_full.send_paid_message_stars = 25;
        channel_full.invite_link = DialogInviteLink::from_telegram(
            Some(telegram_api::ExportedChatInvite::chatInviteExported(
                exported_invite(),
            )),
            false,
            false,
            "test",
        );
        channel_full.bot_verification =
            BotVerification::from_telegram(telegram_api::BotVerification {
                bot_id: 3000000008,
                icon: 99,
                description: Some(telegram_api::TextWithEntities {
                    text: "stamp".to_owned(),
                    entities: Vec::new(),
                }),
            })
            .into();
        channel_full.guard_bot_user_id = UserId::new(3000000009);
        channel_full.sticker_set_id = StickerSetId::new(111);
        channel_full.emoji_sticker_set_id = StickerSetId::new(222);
        channel_full.linked_channel_id = ChannelId::new(1_000_000_000_004);
        channel_full.monoforum_channel_id = ChannelId::new(1_002_147_483_651);
        channel_full.linked_community_id = CommunityId::new(77);
        channel_full.main_profile_tab = ProfileTab::Posts;
        channel_full.stats_dc_id = DcId::create(2);
        channel_full.slow_mode_delay = 10;
        channel_full.slow_mode_next_send_date = 1_700_000_000;
        channel_full.migrated_from_max_message_id = MessageId::new(1234);
        channel_full.migrated_from_chat_id = ChatId::new(12345);
        channel_full.bot_user_ids = vec![UserId::new(3000000010), UserId::new(3000000011)];
        channel_full.can_get_participants = true;
        channel_full.has_hidden_participants = true;
        channel_full.is_all_history_available = false;
        channel_full.expires_at = ctx.now + 60.0;
        // the repair tail re-derives this from the stats dc on parse
        channel_full.is_can_view_statistics_inited = false;
        channel_full.can_view_statistics = false;

        let parsed = round_trip(
            |out| channel_full.store(out, &ctx),
            |input| ChannelFull::parse_value(input, &ctx).unwrap(),
        );
        // the repair tail runs on parse; the source record had the flag off
        assert!(parsed.can_view_statistics);
        assert!(parsed.can_have_sponsored_messages);

        // a blob without the flags2 block has no sponsored-messages flag at
        // all — the repair tail turns it back on, exactly like the C++
        // reading a pre-flags2 database row
        let mut out = Vec::new();
        crate::store_parse::store_log_event_version(&mut out);
        store_flags(
            &mut out,
            [
                false, // has_description
                false, // has_administrator_count
                false, // has_restricted_count
                false, // has_banned_count
                false, // legacy_has_invite_link
                false, // has_sticker_set
                false, // has_linked_channel_id
                false, // has_migrated_from_max_message_id
                false, // has_migrated_from_chat_id
                false, // can_get_participants
                false, // can_set_username
                false, // can_set_sticker_set
                false, // legacy_can_view_statistics
                true,  // is_all_history_available
                false, // can_set_location
                false, // has_location
                false, // has_bot_user_ids
                false, // is_slow_mode_enabled
                false, // is_slow_mode_delay_active
                true,  // has_stats_dc_id
                false, // has_photo
                false, // is_can_view_statistics_inited
                false, // can_view_statistics
                false, // legacy_has_active_group_call_id
                false, // has_invite_link
                false, // has_bot_commands
                false, // can_be_deleted
                false, // has_aggressive_anti_spam_enabled
                false, // has_hidden_participants
                false, // has_flags2
            ],
        );
        store_i32(&mut out, 0); // participant_count
        TimeCtx::store_time(&mut out, 0.0, &ctx);
        store_i32(&mut out, DcId::create(2).get_raw_id()); // stats_dc_id
        let parsed = ChannelFull::parse_value(&out, &ctx).unwrap();
        assert!(parsed.can_have_sponsored_messages);
        assert!(parsed.can_view_statistics);
        assert!(parsed.stats_dc_id.is_exact());

        // store the parsed record (with the repaired flags) — now it is a
        // fixed point and the round trip is exact
        let reparsed = round_trip(
            |out| parsed.store(out, &ctx),
            |input| ChannelFull::parse_value(input, &ctx).unwrap(),
        );
        assert_eq!(reparsed, parsed);

        // the ProfileTab persisted-form conversions
        assert_eq!(ProfileTab::from_raw(0), ProfileTab::Default);
        assert_eq!(ProfileTab::from_raw(8), ProfileTab::Gifs);
        assert_eq!(ProfileTab::from_raw(99), ProfileTab::Default);
        assert_eq!(ProfileTab::Posts.as_raw(), 1);
    }

    #[test]
    fn dialog_location_round_trips() {
        let mut location = DialogLocation::default();
        assert!(location.empty());
        let mut out = Vec::new();
        location.store(&mut out);
        let mut parser = Parser::new(&out);
        assert_eq!(DialogLocation::parse_impl(&mut parser).unwrap(), location);
        parser.fetch_end().unwrap();

        // an address-only location is non-empty but keeps an empty Location
        location.address_ = "somewhere".to_owned();
        assert!(!location.empty());
        let mut out = Vec::new();
        location.store(&mut out);
        let mut parser = Parser::new(&out);
        let parsed = DialogLocation::parse_impl(&mut parser).unwrap();
        assert_eq!(parsed.get_address(), "somewhere");
        assert!(parsed.location_.is_empty());
        assert!(!parsed.empty());
    }

    #[test]
    fn bot_verification_record_matches_the_cpp() {
        let verification = BotVerification::from_telegram(telegram_api::BotVerification {
            bot_id: 3000000008,
            icon: 99,
            description: Some(telegram_api::TextWithEntities {
                text: "stamp".to_owned(),
                entities: Vec::new(),
            }),
        });
        assert!(verification.is_valid());
        assert!(
            !BotVerification::from_telegram(telegram_api::BotVerification {
                bot_id: 0,
                icon: 99,
                description: None,
            })
            .is_valid()
        );
        // an invalid record is dropped by the get_bot_verification gate
        assert!(
            BotVerification::get_bot_verification(Some(telegram_api::BotVerification {
                bot_id: 0,
                icon: 0,
                description: None,
            }))
            .is_none()
        );
        assert!(BotVerification::get_bot_verification(None).is_none());

        // the byte format: the empty flag block, the two ids, the string
        let mut out = Vec::new();
        verification.store(&mut out);
        assert_eq!(
            hex(&out),
            "00000000085ed0b2000000006300000000000000057374616d700000"
        );
        let mut parser = Parser::new(&out);
        assert_eq!(
            BotVerification::parse_impl(&mut parser).unwrap(),
            verification
        );
        parser.fetch_end().unwrap();
    }

    // ---- ChatManager persistence plane (M.5.12) ----

    /// Opens a throwaway database; phase 2 of the reopen tests calls it
    /// again over the same (now populated) directory.
    fn open_db(dir: &str, chat_info_db: bool) -> (Global, crate::option_manager::OptionManager) {
        let _ = std::fs::create_dir_all(dir);
        let opened = tddb::td_db::TdDb::open(tddb::td_db::Parameters {
            database_directory: dir.to_owned(),
            use_message_database: chat_info_db,
            use_chat_info_database: chat_info_db,
            ..Default::default()
        })
        .unwrap();
        let mut global = Global::new();
        global.init(opened.database);
        global.binlog_pmc().set("auth", "ok");
        let options = crate::option_manager::OptionManager::new(global.is_test_dc());
        (global, options)
    }

    fn db_dir(name: &str) -> String {
        std::env::temp_dir()
            .join(format!(
                "td-rs-m512-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ))
            .to_string_lossy()
            .into_owned()
    }

    fn close(global: &mut Global) {
        global.close_all(false);
    }

    fn make_env<'a>(
        global: &'a mut Global,
        options: &'a mut crate::option_manager::OptionManager,
    ) -> UserEnv<'a> {
        UserEnv {
            global,
            options,
            auth_is_bot: false,
        }
    }

    type RecordedPromise = (
        Promise<()>,
        std::sync::Arc<std::sync::Mutex<Option<tdutils::status::Result<()>>>>,
    );

    /// A `Promise<()>` that records its result for the test to inspect.
    fn recorded_unit_promise() -> RecordedPromise {
        let recorded = std::sync::Arc::new(std::sync::Mutex::new(None));
        let sink = recorded.clone();
        let promise = tdutils::promise::PromiseCreator::lambda_result(
            move |result: tdutils::status::Result<()>| {
                *sink.lock().unwrap() = Some(result);
            },
        );
        (promise, recorded)
    }

    fn assert_unit_error(
        recorded: &std::sync::Mutex<Option<tdutils::status::Result<()>>>,
        expected: &str,
    ) {
        match &*recorded.lock().unwrap() {
            Some(tdutils::status::Result::Err(status)) => assert_eq!(status.message(), expected),
            other => panic!("expected the error {expected:?}, got {other:?}"),
        }
    }

    fn sample_chat() -> Chat {
        let mut chat = Chat::new();
        chat.title = "group".to_owned();
        chat.participant_count = 7;
        chat.date = 1_600_000_000;
        chat.version = 3;
        chat.status = DialogParticipantStatus::creator(true, false, String::new());
        chat.is_active = true;
        chat.noforwards = true;
        chat
    }

    fn sample_channel() -> Channel {
        let ctx = test_time_ctx();
        let mut channel = Channel::new();
        channel.access_hash = -987654321098765432;
        channel.title = "supergroup".to_owned();
        channel.is_megagroup = true;
        channel.date = 1_600_000_000;
        channel.participant_count = 11;
        channel.status = DialogParticipantStatus::member(0, String::new());
        channel.max_read_story_id = StoryId::new(28);
        channel.max_active_story_id_next_reload_time = ctx.now + 100.0;
        channel.cache_version = Channel::CACHE_VERSION;
        channel.noforwards = true;
        channel
    }

    #[test]
    fn chat_id_decoders_match_the_cpp() {
        let empty = telegram_api::Chat::chatEmpty(telegram_api::chatEmpty { id: 42 });
        let chat = telegram_api::Chat::chat(telegram_api::chat {
            flags: 0,
            ..telegram_api::chat::default()
        });
        let forbidden = telegram_api::Chat::chatForbidden(telegram_api::chatForbidden {
            id: 43,
            ..telegram_api::chatForbidden::default()
        });
        let channel = telegram_api::Chat::channel(telegram_api::channel {
            id: 1_000_000_000_044,
            ..telegram_api::channel::default()
        });
        let channel_forbidden =
            telegram_api::Chat::channelForbidden(telegram_api::channelForbidden {
                id: 1_000_000_000_045,
                ..telegram_api::channelForbidden::default()
            });

        assert_eq!(ChatManager::get_chat_id(&empty), ChatId::new(42));
        assert_eq!(ChatManager::get_channel_id(&empty), ChannelId::new(0));
        assert_eq!(ChatManager::get_chat_id(&chat), ChatId::new(0));
        assert_eq!(ChatManager::get_chat_id(&forbidden), ChatId::new(43));
        assert_eq!(
            ChatManager::get_channel_id(&channel),
            ChannelId::new(1_000_000_000_044)
        );
        assert_eq!(
            ChatManager::get_channel_id(&channel_forbidden),
            ChannelId::new(1_000_000_000_045)
        );

        // the dialog id prefers the channel form and falls back to the chat
        assert_eq!(
            ChatManager::get_dialog_id(&channel),
            DialogId::from_channel_id(ChannelId::new(1_000_000_000_044))
        );
        assert_eq!(
            ChatManager::get_dialog_id(&forbidden),
            DialogId::from_chat_id(ChatId::new(43))
        );
    }

    #[test]
    fn chat_database_row_round_trips_across_reopen() {
        let dir = db_dir("chat-row");
        let _ = std::fs::remove_dir_all(&dir);
        let chat_id = ChatId::new(777);

        // Phase 1: save the chat; the row lands in the common kv and (the
        // binlog mirror being a stub) no binlog event is left behind.
        {
            let (mut global, mut options) = open_db(&dir, true);
            let mut cm = ChatManager::new();
            {
                let mut env = make_env(&mut global, &mut options);
                let chat = cm.add_chat(chat_id);
                *chat = sample_chat();
                cm.save_chat(&mut env, chat_id, false);
                assert!(cm.get_chat(chat_id).unwrap().is_saved);
            }
            assert!(
                !global.td_db().common_kv().get(b"gr777").is_empty(),
                "the gr row is durable"
            );
            close(&mut global);
        }

        // Phase 2: the chat is absent from memory and loads through the
        // force path; an unknown chat stays absent after its empty load.
        {
            let (mut global, mut options) = open_db(&dir, true);
            let mut cm = ChatManager::new();
            let mut um = UserManager::new();
            let mut env = make_env(&mut global, &mut options);
            assert!(
                cm.get_chat_force(&mut um, &mut env, ChatId::new(778), "test")
                    .is_none(),
                "an unknown chat stays absent"
            );
            let loaded = cm
                .get_chat_force(&mut um, &mut env, chat_id, "test")
                .unwrap();
            assert!(loaded.is_saved);
            assert_eq!(loaded.title, "group");
            assert_eq!(loaded.participant_count, 7);
            assert!(loaded.status.is_creator());
            assert!(loaded.noforwards);
            close(&mut global);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn channel_database_row_recurses_into_the_monoforum_channel() {
        let dir = db_dir("channel-row");
        let _ = std::fs::remove_dir_all(&dir);
        let channel_id = ChannelId::new(900_000_000_100);
        let monoforum_id = ChannelId::new(1_002_147_483_900);

        // Phase 1: the channel names its monoforum channel; both save.
        {
            let (mut global, mut options) = open_db(&dir, true);
            let mut cm = ChatManager::new();
            {
                let mut env = make_env(&mut global, &mut options);
                let channel = cm.add_channel(channel_id);
                *channel = sample_channel();
                channel.monoforum_channel_id = monoforum_id;
                cm.save_channel(&mut env, channel_id, false);

                let monoforum = cm.add_channel(monoforum_id);
                *monoforum = sample_channel();
                monoforum.title = "monoforum".to_owned();
                monoforum.is_megagroup = false;
                monoforum.monoforum_channel_id = ChannelId::default();
                cm.save_channel(&mut env, monoforum_id, false);
            }
            close(&mut global);
        }

        // Phase 2: loading the channel pulls the monoforum channel in with
        // it; an unknown channel stays absent.
        {
            let (mut global, mut options) = open_db(&dir, true);
            let mut cm = ChatManager::new();
            let mut um = UserManager::new();
            let mut env = make_env(&mut global, &mut options);
            assert!(
                cm.get_channel_force(
                    &mut um,
                    &mut env,
                    ChannelId::new(900_000_000_200),
                    "test",
                    false
                )
                .is_none()
            );
            let loaded = cm
                .get_channel_force(&mut um, &mut env, channel_id, "test", false)
                .unwrap();
            assert_eq!(loaded.title, "supergroup");
            // megagroups force show_message_sender on parse
            assert!(loaded.show_message_sender);
            assert!(
                cm.get_channel(monoforum_id).is_some(),
                "the monoforum channel loaded recursively"
            );
            assert_eq!(cm.get_channel(monoforum_id).unwrap().title, "monoforum");
            close(&mut global);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_migrated_to_channel_gap_is_reported_not_fatal() {
        let dir = db_dir("migrated-gap");
        let _ = std::fs::remove_dir_all(&dir);
        let chat_id = ChatId::new(888);
        let missing_channel_id = ChannelId::new(900_000_000_300);

        // Phase 1: a chat migrated to a channel that is not in the cache.
        {
            let (mut global, mut options) = open_db(&dir, true);
            let mut cm = ChatManager::new();
            {
                let mut env = make_env(&mut global, &mut options);
                let chat = cm.add_chat(chat_id);
                *chat = sample_chat();
                chat.migrated_to_channel_id = missing_channel_id;
                cm.save_chat(&mut env, chat_id, false);
            }
            close(&mut global);
        }

        // Phase 2: the load succeeds; the migrated-to lookup reports the
        // gap (the C++ LOG(ERROR)) and the missing channel stays absent.
        {
            let (mut global, mut options) = open_db(&dir, true);
            let mut cm = ChatManager::new();
            let mut um = UserManager::new();
            let mut env = make_env(&mut global, &mut options);
            assert!(
                cm.get_chat_force(&mut um, &mut env, chat_id, "test")
                    .is_some()
            );
            assert!(!cm.have_channel_force(&mut um, &mut env, missing_channel_id, "test"));
            close(&mut global);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn chat_save_respects_the_database_switch() {
        // use_chat_info_database() = false: the save is a no-op and no row
        // lands.
        let dir = db_dir("no-chat-db");
        let _ = std::fs::remove_dir_all(&dir);
        let (mut global, mut options) = open_db(&dir, false);
        let mut cm = ChatManager::new();
        {
            let mut env = make_env(&mut global, &mut options);
            let chat = cm.add_chat(ChatId::new(999));
            *chat = sample_chat();
            cm.save_chat(&mut env, ChatId::new(999), false);
        }
        assert!(global.td_db().common_kv().get(b"gr999").is_empty());
        close(&mut global);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn chat_log_event_replays_across_reopen() {
        let dir = db_dir("chat-log");
        let _ = std::fs::remove_dir_all(&dir);
        let chat_id = ChatId::new(555);

        // Phase 1: the binlog mirror was written, the sqlite row never
        // landed — the crash window.
        {
            let (mut global, _options) = open_db(&dir, true);
            let mut chat = sample_chat();
            let mut data = Vec::new();
            store_chat_log_event(&mut data, chat_id, &chat);
            global
                .td_db()
                .binlog()
                .add(tddb::td_db::LOG_EVENT_HANDLER_TYPE_CHATS, &data);
            chat.title = "discarded".to_owned();
            // no sqlite write, straight to close — the crash
            close(&mut global);
        }

        // Phase 2: reopen — TdDb collected the Chats event; the replay
        // restores the chat and its save retires the mirror.
        {
            let opened = tddb::td_db::TdDb::open(tddb::td_db::Parameters {
                database_directory: dir.clone(),
                ..Default::default()
            })
            .unwrap();
            assert_eq!(opened.chat_events.len(), 1, "the Chats event replays");
            let event = opened.chat_events[0].clone();
            drop(opened);

            let (mut global, mut options) = open_db(&dir, true);
            let mut cm = ChatManager::new();
            let mut um = UserManager::new();
            {
                let mut env = make_env(&mut global, &mut options);
                cm.on_binlog_chat_event(&mut um, &mut env, &event);
            }
            let c = cm.get_chat(chat_id).expect("replayed chat");
            assert_eq!(c.title, "group");
            assert_eq!(c.participant_count, 7);
            assert!(c.is_active);
            assert!(c.is_saved);
            assert_eq!(c.log_event_id, 0, "the save retired the mirror");
            let row = global.td_db().common_kv().get(b"gr555");
            assert!(!row.is_empty(), "the replay saved the sqlite row");
            assert_eq!(Chat::parse_value(&row).unwrap().title, "group");
            // The C++ re-delivery path (skip + erase of an already-erased
            // event id) is not exercised here: the Rust binlog lacks the
            // C++'s idempotent re-erase, a tddb-layer divergence tracked
            // for the binlog polish step.
            close(&mut global);
        }

        // Phase 3: reopen again — no Chats events left; the chat loads
        // through the manager's database path.
        {
            let opened = tddb::td_db::TdDb::open(tddb::td_db::Parameters {
                database_directory: dir.clone(),
                ..Default::default()
            })
            .unwrap();
            assert!(opened.chat_events.is_empty(), "the mirror retired");
            drop(opened);

            let (mut global, mut options) = open_db(&dir, true);
            let mut cm = ChatManager::new();
            let mut um = UserManager::new();
            let mut env = make_env(&mut global, &mut options);
            let loaded = cm
                .get_chat_force(&mut um, &mut env, chat_id, "test")
                .unwrap();
            assert_eq!(loaded.title, "group");
            assert!(loaded.is_saved);
            close(&mut global);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn channel_log_event_replays_across_reopen() {
        let dir = db_dir("channel-log");
        let _ = std::fs::remove_dir_all(&dir);
        let channel_id = ChannelId::new(900_000_000_500);

        // Phase 1: the crash window — the mirror only.
        {
            let (mut global, _options) = open_db(&dir, true);
            let channel = sample_channel();
            let ctx = test_time_ctx();
            let mut data = Vec::new();
            store_channel_log_event(&mut data, channel_id, &channel, &ctx);
            global
                .td_db()
                .binlog()
                .add(tddb::td_db::LOG_EVENT_HANDLER_TYPE_CHANNELS, &data);
            close(&mut global);
        }

        // Phase 2: the replay restores the channel and saves its row.
        {
            let opened = tddb::td_db::TdDb::open(tddb::td_db::Parameters {
                database_directory: dir.clone(),
                ..Default::default()
            })
            .unwrap();
            assert_eq!(opened.channel_events.len(), 1, "the Channels event replays");
            let event = opened.channel_events[0].clone();
            drop(opened);

            let (mut global, mut options) = open_db(&dir, true);
            let mut cm = ChatManager::new();
            let mut um = UserManager::new();
            {
                let mut env = make_env(&mut global, &mut options);
                cm.on_binlog_channel_event(&mut um, &mut env, &event);
            }
            let c = cm.get_channel(channel_id).expect("replayed channel");
            assert_eq!(c.title, "supergroup");
            assert_eq!(c.access_hash, -987654321098765432);
            assert!(c.is_saved);
            assert_eq!(c.log_event_id, 0, "the save retired the mirror");
            let row = global.td_db().common_kv().get(b"ch900000000500");
            assert!(!row.is_empty(), "the replay saved the sqlite row");
            close(&mut global);
        }

        // Phase 3: no Channels events left; the row loads.
        {
            let opened = tddb::td_db::TdDb::open(tddb::td_db::Parameters {
                database_directory: dir.clone(),
                ..Default::default()
            })
            .unwrap();
            assert!(opened.channel_events.is_empty(), "the mirror retired");
            drop(opened);

            let (mut global, mut options) = open_db(&dir, true);
            let mut cm = ChatManager::new();
            let mut um = UserManager::new();
            let mut env = make_env(&mut global, &mut options);
            let loaded = cm
                .get_channel_force(&mut um, &mut env, channel_id, "test", false)
                .unwrap();
            assert_eq!(loaded.title, "supergroup");
            close(&mut global);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- ChatManager timeouts + full planes (M.5.14–M.5.17) ----

    fn sample_chat_full() -> ChatFull {
        let mut chat_full = ChatFull::new();
        chat_full.description = "about".to_owned();
        chat_full.creator_user_id = UserId::new(0);
        chat_full.participants = Vec::new();
        chat_full.version = 3;
        // the creator-chat gate requires a present link; its admin_id
        // sanitizes to the invalid user, so the load dependency resolves
        // without a loadable user
        let mut zero_admin = exported_invite();
        zero_admin.admin_id = 0;
        chat_full.invite_link = DialogInviteLink::from_telegram(
            Some(telegram_api::ExportedChatInvite::chatInviteExported(
                zero_admin,
            )),
            false,
            false,
            "test",
        );
        chat_full
    }

    fn sample_channel_full() -> ChannelFull {
        let mut channel_full = ChannelFull::new();
        channel_full.description = "supergroup about".to_owned();
        channel_full.participant_count = 11;
        channel_full.administrator_count = 2;
        channel_full.slow_mode_delay = 30;
        // the sample channel's member status requires no invite link; a
        // present one would need a loadable creator user to resolve
        channel_full
    }

    #[test]
    fn chat_full_plane_round_trips_through_reopen() {
        let dir = db_dir("m514-chat-full-plane");
        let chat_id = ChatId::new(77);
        {
            let (mut global, mut options) = open_db(&dir, true);
            let mut cm = ChatManager::new();
            let mut um = UserManager::new();
            let mut env = make_env(&mut global, &mut options);
            cm.add_chat(chat_id).clone_from(&sample_chat());
            // a member-status chat: the load's invite-link gate drops
            // fulls of invite-link-managing chats without a valid stored
            // link (a valid link would need a loadable creator user)
            cm.get_chat_mut(chat_id).unwrap().status =
                DialogParticipantStatus::member(0, String::new());
            cm.update_chat(&mut um, &mut env, chat_id, false, false);
            cm.save_chat(&mut env, chat_id, false);
            *cm.add_chat_full(chat_id) = sample_chat_full();
            cm.save_chat_full(&mut env, chat_id);
            close(&mut global);
        }
        {
            let (mut global, mut options) = open_db(&dir, true);
            let mut cm = ChatManager::new();
            let mut um = UserManager::new();
            let mut env = make_env(&mut global, &mut options);
            // the chat itself loads first (the C++ caller contract)
            assert!(cm.have_chat_force(&mut um, &mut env, chat_id, "test"));
            let chat_full = cm
                .get_chat_full_force(&mut um, &mut env, chat_id, "test")
                .expect("full loads after the chat");
            assert_eq!(chat_full.description, "about");
            assert_eq!(chat_full.version, 3);
            assert!(chat_full.is_update_chat_full_sent);
            // the load emits updateBasicGroupFullInfo (the C++ load path
            // ends in update_chat_full(from_database = true))
            let updates = cm.take_updates();
            assert!(
                updates
                    .iter()
                    .any(|u| matches!(u, td_api::Update::updateBasicGroupFullInfo(_)))
            );
            close(&mut global);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn channel_full_plane_round_trips_and_arms_the_slow_mode_deadline() {
        let dir = db_dir("m514-channel-full-plane");
        let channel_id = ChannelId::new(100_000_000_078);
        {
            let (mut global, mut options) = open_db(&dir, true);
            let mut cm = ChatManager::new();
            let mut um = UserManager::new();
            let mut env = make_env(&mut global, &mut options);
            cm.add_channel(channel_id).clone_from(&sample_channel());
            cm.update_channel(&mut um, &mut env, channel_id, false, false);
            cm.save_channel(&mut env, channel_id, false);
            let mut channel_full = sample_channel_full();
            channel_full.slow_mode_next_send_date = env.global.unix_time() + 100;
            channel_full.is_slow_mode_next_send_date_changed = true;
            *cm.add_channel_full(channel_id) = channel_full;
            cm.save_channel_full(&mut env, channel_id);
            close(&mut global);
        }
        {
            let (mut global, mut options) = open_db(&dir, true);
            let mut cm = ChatManager::new();
            let mut um = UserManager::new();
            let mut env = make_env(&mut global, &mut options);
            assert!(cm.have_channel_force(&mut um, &mut env, channel_id, "test"));
            // the stored slow mode survives the load only on a channel with
            // slow mode enabled (C++ drops it from the full otherwise)
            cm.get_channel_mut(channel_id).unwrap().is_slow_mode_enabled = true;
            let channel_full = cm
                .get_channel_full_force(&mut um, &mut env, channel_id, true, "test", false)
                .expect("full loads after the channel");
            assert_eq!(channel_full.description, "supergroup about");
            assert_eq!(channel_full.participant_count, 11);
            assert!(!channel_full.invite_link.is_valid());
            assert!(channel_full.is_update_channel_full_sent);
            // update_channel_full(from_database = true) ran the slow-mode
            // block: the deadline is armed at next_send_date - server_time
            assert!(cm.slow_mode_delay_timeouts.contains_key(&channel_id.get()));
            let updates = cm.take_updates();
            assert!(
                updates
                    .iter()
                    .any(|u| matches!(u, td_api::Update::updateSupergroupFullInfo(_)))
            );

            // fire the slow-mode deadline: next_send_date resets to 0, the
            // timeout cancels, and the full update re-emits
            cm.slow_mode_delay_timeouts
                .insert(channel_id.get(), tdutils::time::now() - 1.0);
            cm.tick_timeouts(&mut um, &mut env);
            let channel_full = cm.get_channel_full_const(channel_id).unwrap();
            assert_eq!(channel_full.slow_mode_next_send_date, 0);
            assert!(!cm.slow_mode_delay_timeouts.contains_key(&channel_id.get()));
            let updates = cm.take_updates();
            assert!(
                updates
                    .iter()
                    .any(|u| matches!(u, td_api::Update::updateSupergroupFullInfo(_)))
            );
            close(&mut global);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unban_deadline_updates_the_status_and_invalidates_the_full() {
        let (mut global, mut options) = open_db(&db_dir("m514-unban"), false);
        let mut cm = ChatManager::new();
        let mut um = UserManager::new();
        let mut env = make_env(&mut global, &mut options);
        let channel_id = ChannelId::new(100_000_000_079);
        {
            let c = cm.add_channel(channel_id);
            c.clone_from(&sample_channel());
            c.status = DialogParticipantStatus::restricted(
                RestrictedRights::restrict_all(),
                true,
                env.global.unix_time() - 10,
                ChannelType::Megagroup,
                String::new(),
            );
            c.is_update_supergroup_sent = true;
        }
        let old_status = cm.get_channel(channel_id).unwrap().status.clone();

        // arm the unban deadline in the past (the deadline map is the
        // MultiTimeout stand-in); the C++ unified queue pops the unban
        // callback before the later emoji deadline
        let t0 = tdutils::time::now();
        cm.channel_unban_timeouts.insert(channel_id.get(), t0 - 1.0);
        assert_eq!(cm.next_deadline(), Some(t0 - 1.0));
        cm.tick_timeouts(&mut um, &mut env);

        // the restriction expired: update_channel consumed the diff flags,
        // emitted updateSupergroup, and with no full loaded the
        // invalidation registered instead
        let c = cm.get_channel(channel_id).unwrap();
        assert_ne!(c.status, old_status);
        assert!(!c.is_status_changed);
        assert!(!c.is_changed);
        assert!(c.is_update_supergroup_sent);
        assert!(cm.invalidated_channels_full.contains(&channel_id.get()));
        assert!(cm.channel_unban_timeouts.is_empty());
        assert!(cm.channel_emoji_status_timeouts.is_empty());
        let updates = cm.take_updates();
        assert!(
            updates
                .iter()
                .any(|u| matches!(u, td_api::Update::updateSupergroup(_)))
        );
        close(&mut global);
        let _ = std::fs::remove_dir_all(db_dir("m514-unban"));
    }

    #[test]
    fn chat_full_load_repairs_a_corrupt_row() {
        let dir = db_dir("m514-corrupt-grf");
        let chat_id = ChatId::new(88);
        {
            let (mut global, mut options) = open_db(&dir, true);
            let mut cm = ChatManager::new();
            let mut env = make_env(&mut global, &mut options);
            cm.add_chat(chat_id).clone_from(&sample_chat());
            cm.save_chat(&mut env, chat_id, false);
            env.global.td_db().common_kv().set(
                &ChatManager::get_chat_full_database_key(chat_id),
                &[1, 2, 3, 4, 5],
            );
            close(&mut global);
        }
        {
            let (mut global, mut options) = open_db(&dir, true);
            let mut cm = ChatManager::new();
            let mut um = UserManager::new();
            let mut env = make_env(&mut global, &mut options);
            assert!(cm.have_chat_force(&mut um, &mut env, chat_id, "test"));
            // the parse fails: the row is erased and the full is unknown;
            // the one-shot gate keeps answering None afterwards
            assert!(
                cm.get_chat_full_force(&mut um, &mut env, chat_id, "test")
                    .is_none()
            );
            assert!(
                cm.get_chat_full_force(&mut um, &mut env, chat_id, "test")
                    .is_none()
            );
            assert!(
                env.global
                    .td_db()
                    .common_kv()
                    .get(&ChatManager::get_chat_full_database_key(chat_id))
                    .is_empty()
            );
            close(&mut global);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn chat_full_load_drops_a_full_without_the_required_invite_link() {
        let dir = db_dir("m514-invite-gate");
        let chat_id = ChatId::new(89);
        {
            let (mut global, mut options) = open_db(&dir, true);
            let mut cm = ChatManager::new();
            let mut env = make_env(&mut global, &mut options);
            let mut chat = sample_chat(); // creator + active: can_manage_invite_links
            chat.status = DialogParticipantStatus::creator(true, false, String::new());
            cm.add_chat(chat_id).clone_from(&chat);
            cm.save_chat(&mut env, chat_id, false);
            *cm.add_chat_full(chat_id) = sample_chat_full(); // no invite link
            cm.save_chat_full(&mut env, chat_id);
            close(&mut global);
        }
        {
            let (mut global, mut options) = open_db(&dir, true);
            let mut cm = ChatManager::new();
            let mut um = UserManager::new();
            let mut env = make_env(&mut global, &mut options);
            assert!(cm.have_chat_force(&mut um, &mut env, chat_id, "test"));
            // need_invite_link != have_invite_link with need == true: the
            // row stays but the full is treated as absent
            assert!(
                cm.get_chat_full_force(&mut um, &mut env, chat_id, "test")
                    .is_none()
            );
            assert!(
                !env.global
                    .td_db()
                    .common_kv()
                    .get(&ChatManager::get_chat_full_database_key(chat_id))
                    .is_empty()
            );
            close(&mut global);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn query_senders_answer_the_cpp_validation_surfaces() {
        let (mut global, mut options) = open_db(&db_dir("m514-queries"), false);
        let mut cm = ChatManager::new();
        let mut um = UserManager::new();
        let mut env = make_env(&mut global, &mut options);

        let (p1, r1) = recorded_unit_promise();
        cm.load_chat_full(&mut um, &mut env, ChatId::new(0), false, p1, "test");
        assert_unit_error(&r1, "Group not found");

        // the C++ load_chat_full consults the in-memory chat first: an
        // unknown chat answers the 400 without any query
        let (p2, r2) = recorded_unit_promise();
        cm.load_chat_full(&mut um, &mut env, ChatId::new(91), false, p2, "test");
        assert_unit_error(&r2, "Group not found");

        // with the chat known but no full anywhere, the get query answers
        // the net-step surface
        cm.add_chat(ChatId::new(91)).clone_from(&sample_chat());
        let (p2b, r2b) = recorded_unit_promise();
        cm.load_chat_full(&mut um, &mut env, ChatId::new(91), false, p2b, "test");
        assert_unit_error(&r2b, "Network is not implemented yet");

        // reload_chat_full forwards blindly in the C++ (cpp:8509-8511);
        // the "Can't access the chat" surface is the GetFullChatQuery
        // on_error mapping (cpp:1086), which lands with the net step
        let (p3, r3) = recorded_unit_promise();
        cm.reload_chat_full(&mut env, ChatId::new(92), p3, "test");
        assert_unit_error(&r3, "Network is not implemented yet");

        let (p4, r4) = recorded_unit_promise();
        cm.load_channel_full(&mut um, &mut env, ChannelId::new(0), false, p4, "test");
        assert_unit_error(&r4, "Supergroup not found");

        // the channel twin needs the channel + read access before the net
        cm.on_get_chat_full_failed(&mut env, ChatId::new(5));
        cm.on_get_channel_full_failed(&mut um, &mut env, ChannelId::new(5));
        close(&mut global);
        let _ = std::fs::remove_dir_all(db_dir("m514-queries"));
    }

    #[test]
    fn file_source_ids_follow_the_cpp_edges() {
        let (mut global, mut options) = open_db(&db_dir("m514-file-sources"), false);
        let mut cm = ChatManager::new();
        let mut env = make_env(&mut global, &mut options);
        let chat_id = ChatId::new(93);

        // no full in memory: the stand-in registry hands out sources
        let first = cm.get_chat_full_file_source_id(&mut env, chat_id);
        assert!(first.is_valid());
        assert_eq!(cm.get_chat_full_file_source_id(&mut env, chat_id), first);
        assert!(
            !cm.get_chat_full_file_source_id(&mut env, ChatId::new(0))
                .is_valid()
        );

        // a full that was never sent keeps its (still invalid) source;
        // after the update was sent the C++ answers the invalid id
        cm.add_chat_full(chat_id);
        let via_full = cm.get_chat_full_file_source_id(&mut env, chat_id);
        assert!(!via_full.is_valid());

        let channel_id = ChannelId::new(100_000_000_094);
        let channel_source = cm.get_channel_full_file_source_id(&mut env, channel_id);
        assert!(channel_source.is_valid());
        close(&mut global);
        let _ = std::fs::remove_dir_all(db_dir("m514-file-sources"));
    }

    #[test]
    fn id_objects_queue_the_unknown_updates_once() {
        let (mut global, mut options) = open_db(&db_dir("m514-unknown"), false);
        let mut cm = ChatManager::new();
        let mut _env = make_env(&mut global, &mut options);
        let chat_id = ChatId::new(95);
        let channel_id = ChannelId::new(100_000_000_096);

        assert_eq!(cm.get_basic_group_id_object(chat_id, "test"), 95);
        assert_eq!(cm.get_basic_group_id_object(chat_id, "test"), 95);
        assert_eq!(
            cm.get_supergroup_id_object(channel_id, "test"),
            channel_id.get()
        );
        assert_eq!(
            cm.get_supergroup_id_object(channel_id, "test"),
            channel_id.get()
        );

        let updates = cm.take_updates();
        let unknown_group = updates
            .iter()
            .filter(|u| matches!(u, td_api::Update::updateBasicGroup(_)))
            .count();
        let unknown_super = updates
            .iter()
            .filter(|u| matches!(u, td_api::Update::updateSupergroup(_)))
            .count();
        assert_eq!(unknown_group, 1);
        assert_eq!(unknown_super, 1);

        // known ids never queue
        cm.add_chat(chat_id).clone_from(&sample_chat());
        cm.add_min_channel(channel_id, MinChannel::default());
        assert_eq!(cm.get_basic_group_id_object(chat_id, "test"), 95);
        assert_eq!(
            cm.get_supergroup_id_object(channel_id, "test"),
            channel_id.get()
        );
        assert!(cm.take_updates().is_empty());
        assert!(cm.have_min_channel(channel_id));
        close(&mut global);
        let _ = std::fs::remove_dir_all(db_dir("m514-unknown"));
    }

    #[test]
    fn full_photo_applier_tracks_registered_file_ids() {
        let (mut global, mut options) = open_db(&db_dir("m514-photo"), false);
        let mut cm = ChatManager::new();
        let mut env = make_env(&mut global, &mut options);
        let chat_id = ChatId::new(97);
        cm.add_chat_full(chat_id);
        cm.get_chat_full_mut(chat_id).unwrap().is_changed = false;
        // an empty photo on an empty full: nothing changes
        cm.on_update_chat_full_photo(&mut env, chat_id, Photo::default());
        let f = cm.get_chat_full(chat_id).unwrap();
        assert!(!f.is_changed);
        assert!(f.registered_photo_file_ids.is_empty());
        // empty-vs-empty photos compare equal (invalid file ids both sides)
        assert!(is_same_dialog_photo(
            &Photo::default(),
            &DialogPhoto::default()
        ));
        close(&mut global);
        let _ = std::fs::remove_dir_all(db_dir("m514-photo"));
    }

    #[test]
    fn input_peer_gates_match_the_cpp() {
        let (mut global, mut options) = open_db(db_dir("m514-input-peer").as_str(), false);
        let mut cm = ChatManager::new();
        let env = make_env(&mut global, &mut options);
        let chat_id = ChatId::new(600);
        let channel_id = ChannelId::new(100_000_000_601);
        let monoforum_id = ChannelId::new(100_000_000_602);

        // basic groups: unknown → no peer; Know/Read always; Write needs an
        // active chat; left status blocks Write
        assert!(!cm.have_input_peer_chat(chat_id, AccessRights::Read));
        assert!(
            cm.get_input_peer_chat(chat_id, AccessRights::Read)
                .is_none()
        );
        let mut chat = Chat::new();
        chat.is_active = true;
        chat.status = DialogParticipantStatus::creator(true, false, String::new());
        *cm.add_chat(chat_id) = chat;
        assert!(
            cm.get_input_peer_chat(chat_id, AccessRights::Read)
                .is_some()
        );
        assert!(
            cm.get_input_peer_chat(chat_id, AccessRights::Write)
                .is_some()
        );
        let mut left_chat = Chat::new();
        left_chat.is_active = true;
        left_chat.status = DialogParticipantStatus::left();
        *cm.add_chat(ChatId::new(603)) = left_chat;
        assert!(!cm.have_input_peer_chat(ChatId::new(603), AccessRights::Write));
        assert!(cm.have_input_peer_chat(ChatId::new(603), AccessRights::Read));

        // channels: banned members have no peer; monoforum children resolve
        // through the parent at Read; the from-message branch stays a stub
        let mut banned = sample_channel();
        banned.status =
            DialogParticipantStatus::banned(env.global.unix_time() + 100, String::new());
        *cm.add_channel(channel_id) = banned;
        assert!(!cm.have_input_peer_channel_id(&env, channel_id, AccessRights::Read));
        assert!(
            cm.get_input_peer_channel(&env, channel_id, AccessRights::Read)
                .is_none()
        );

        let mut parent = sample_channel();
        parent.status = DialogParticipantStatus::member(0, String::new());
        parent.is_monoforum = true;
        parent.monoforum_channel_id = monoforum_id;
        *cm.add_channel(monoforum_id) = parent;
        let mut child = sample_channel();
        child.status = DialogParticipantStatus::member(0, String::new());
        child.is_monoforum = true;
        child.monoforum_channel_id = monoforum_id;
        *cm.add_channel(ChannelId::new(100_000_000_603)) = child;
        assert!(cm.have_input_peer_channel_id(
            &env,
            ChannelId::new(100_000_000_603),
            AccessRights::Read
        ));

        let simple = cm.get_simple_input_peer(DialogId::from_channel_id(channel_id));
        assert!(simple.is_some());
        close(&mut global);
        let _ = std::fs::remove_dir_all(db_dir("m514-input-peer"));
    }

    #[test]
    fn chat_dispatcher_routes_and_applies_the_field_matrix() {
        let (mut global, mut options) = open_db(&db_dir("m520-dispatch"), true);
        let mut cm = ChatManager::new();
        let mut um = UserManager::new();
        let mut env = make_env(&mut global, &mut options);
        let chat_id = ChatId::new(700);

        // a creator chat: the field matrix lands in the record, the status
        // ladder picks creator, the versions come straight from the wire
        let chat = telegram_api::Chat::chat(telegram_api::chat {
            flags: (1 << 13) | (1 << 14) | (1 << 18),
            creator: true,
            noforwards: true,
            id: chat_id.get(),
            title: "dispatched".to_owned(),
            participants_count: 12,
            date: 1_700_000_000,
            version: 4,
            admin_rights: Some(telegram_api::ChatAdminRights::default()),
            default_banned_rights: Some(telegram_api::ChatBannedRights::default()),
            ..telegram_api::chat::default()
        });
        cm.on_get_chat(&mut um, &mut env, chat, "test");
        let c = cm.get_chat(chat_id).expect("basic group applied");
        assert_eq!(c.title, "dispatched");
        assert_eq!(c.participant_count, 12);
        assert_eq!(c.version, 4);
        assert_eq!(c.date, 1_700_000_000);
        assert!(c.status.is_creator());
        assert!(c.noforwards);
        assert!(c.is_received_from_server);
        assert_eq!(c.cache_version, Chat::CACHE_VERSION);

        // chatEmpty for an unknown chat changes nothing
        cm.on_get_chat(
            &mut um,
            &mut env,
            telegram_api::Chat::chatEmpty(telegram_api::chatEmpty { id: 999 }),
            "test",
        );
        assert!(!cm.have_chat(ChatId::new(999)));

        // communities are consumed by the CommunityManager stub
        let before = cm.take_updates();
        assert!(!before.is_empty());
        cm.take_updates();
        cm.on_get_chat(
            &mut um,
            &mut env,
            telegram_api::Chat::community(telegram_api::community::default()),
            "test",
        );
        assert!(cm.take_updates().is_empty());
        close(&mut global);
        let _ = std::fs::remove_dir_all(db_dir("m520-dispatch"));
    }

    #[test]
    fn on_get_chats_resolves_the_migrated_pair() {
        let (mut global, mut options) = open_db(&db_dir("m520-chats"), true);
        let mut cm = ChatManager::new();
        let mut um = UserManager::new();
        let mut env = make_env(&mut global, &mut options);
        let chat_id = ChatId::new(701);
        let upgraded = ChannelId::new(702);

        // the basic group first: its migrated-to supergroup does not exist
        // yet and gets force-created as a left megagroup placeholder
        cm.on_get_chats(
            &mut um,
            &mut env,
            vec![
                telegram_api::Chat::chat(telegram_api::chat {
                    left: true,
                    id: chat_id.get(),
                    title: "upgraded group".to_owned(),
                    migrated_to: Some(telegram_api::InputChannel::inputChannel(
                        telegram_api::inputChannel {
                            channel_id: upgraded.get(),
                            access_hash: 4242,
                        },
                    )),
                    ..telegram_api::chat::default()
                }),
                telegram_api::Chat::channel(telegram_api::channel {
                    broadcast: true,
                    id: upgraded.get(),
                    access_hash: 4242,
                    title: "the supergroup".to_owned(),
                    ..telegram_api::channel::default()
                }),
            ],
            "test",
        );
        assert_eq!(
            cm.get_chat(chat_id).expect("group").migrated_to_channel_id,
            upgraded
        );
        let channel = cm.get_channel(upgraded).expect("supergroup");
        assert_eq!(channel.title, "the supergroup");
        assert!(channel.is_received_from_server);
        close(&mut global);
        let _ = std::fs::remove_dir_all(db_dir("m520-chats"));
    }

    #[test]
    fn channel_intake_applies_megagroup_normalization_and_min_cache() {
        let (mut global, mut options) = open_db(&db_dir("m521-channel"), true);
        let mut cm = ChatManager::new();
        let mut um = UserManager::new();
        let mut env = make_env(&mut global, &mut options);
        let channel_id = ChannelId::new(100_000_000_710);

        // megagroup: signatures are forced on and broadcast flags forced off
        cm.on_get_channel(
            &mut um,
            &mut env,
            telegram_api::channel {
                megagroup: true,
                signatures: true,
                flags: 1 << 17,
                id: channel_id.get(),
                access_hash: -77,
                title: "normalized".to_owned(),
                participants_count: 30,
                ..telegram_api::channel::default()
            },
            "test",
        );
        {
            let c = cm.get_channel(channel_id).expect("supergroup applied");
            assert!(c.sign_messages);
            assert!(c.show_message_sender);
            assert!(!c.broadcast_messages_allowed);
            assert!(c.is_megagroup);
            assert!(c.is_received_from_server);
            assert_eq!(c.participant_count, 30);
            assert_eq!(c.access_hash, -77);
        }
        let updates = cm.take_updates();
        assert!(
            updates
                .iter()
                .any(|u| matches!(u, td_api::Update::updateSupergroup(_)))
        );

        // an unknown min-channel lands in the degraded placeholder cache,
        // not in the real channel map
        let min_id = ChannelId::new(100_000_000_711);
        cm.on_get_channel(
            &mut um,
            &mut env,
            telegram_api::channel {
                flags: 1 << 12,
                min: true,
                id: min_id.get(),
                title: "min only".to_owned(),
                ..telegram_api::channel::default()
            },
            "test",
        );
        assert!(!cm.have_channel(min_id));
        let min = cm.get_min_channel(min_id).expect("min placeholder");
        assert_eq!(min.title_, "min only");
        close(&mut global);
        let _ = std::fs::remove_dir_all(db_dir("m521-channel"));
    }

    #[test]
    fn channel_forbidden_zeroes_the_record() {
        let (mut global, mut options) = open_db(&db_dir("m521-forbidden"), true);
        let mut cm = ChatManager::new();
        let mut um = UserManager::new();
        let mut env = make_env(&mut global, &mut options);
        let channel_id = ChannelId::new(100_000_000_712);
        {
            let c = cm.add_channel(channel_id);
            *c = sample_channel();
            c.date = 1_700_000_000;
            c.participant_count = 11;
        }
        cm.on_get_channel_forbidden(
            &mut um,
            &mut env,
            telegram_api::channelForbidden {
                broadcast: true,
                id: channel_id.get(),
                access_hash: -5,
                until_date: 0,
                ..telegram_api::channelForbidden::default()
            },
            "test",
        );
        let c = cm.get_channel(channel_id).unwrap();
        assert!(c.status.is_banned());
        assert_eq!(c.date, 0);
        assert_eq!(c.participant_count, 0);
        assert!(c.is_received_from_server);
        close(&mut global);
        let _ = std::fs::remove_dir_all(db_dir("m521-forbidden"));
    }

    #[test]
    fn message_id_bit_layout_matches_the_cpp() {
        // ordinary server ids: server id shifted by SERVER_ID_SHIFT
        let server = MessageId::from_server_message_id(ServerMessageId::new(7));
        assert_eq!(server.get(), 7i64 << 20);
        assert!(server.is_valid());
        assert!(server.is_server());
        assert!(!server.is_scheduled());
        assert_eq!(server.get_type(), MessageType::Server);
        assert_eq!(server.get_server_message_id(), ServerMessageId::new(7));

        // yet-unsent and local occupy the low type bits
        let yet_unsent = server.get_next_message_id(MessageType::YetUnsent);
        assert_eq!(yet_unsent.get(), (7i64 << 20) + 1);
        assert_eq!(yet_unsent.get_type(), MessageType::YetUnsent);
        assert!(yet_unsent.is_yet_unsent());
        let local = server.get_next_message_id(MessageType::Local);
        assert_eq!(local.get(), (7i64 << 20) + 2);
        assert!(local.is_local());
        assert_eq!(MessageId::new(0).get_type(), MessageType::None);
        assert!(!MessageId::new(-5).is_valid());

        // scheduled ids pack send_date - 2^30 and the server id above bit 3
        let scheduled =
            MessageId::from_scheduled(ScheduledServerMessageId::new(13), (1 << 30) + 5, false);
        assert!(scheduled.is_scheduled());
        assert!(scheduled.is_valid_scheduled());
        assert!(scheduled.is_scheduled_server());
        assert_eq!(
            scheduled.get_scheduled_server_message_id(),
            ScheduledServerMessageId::new(13)
        );
        assert_eq!(scheduled.get_scheduled_message_date(), (1 << 30) + 5);
        assert_eq!(scheduled.get_type(), MessageType::Server);
        // the validation failures reset to 0 (MessageId.cpp:15-24)
        assert_eq!(
            MessageId::from_scheduled(ScheduledServerMessageId::new(13), 1 << 30, false),
            MessageId::new(0)
        );
        assert_eq!(
            MessageId::from_scheduled(ScheduledServerMessageId::new(0), (1 << 30) + 5, false),
            MessageId::new(0)
        );
        assert_eq!(
            MessageId::from_scheduled(ScheduledServerMessageId::new(0), (1 << 30) + 5, true).get(),
            (5i64 << 21) | 4
        );

        // prev/next server rounding (MessageId.h:141-150)
        let middle = MessageId::new((7i64 << 20) + 123);
        assert_eq!(middle.get_prev_server_message_id(), server);
        assert_eq!(
            middle.get_next_server_message_id(),
            MessageId::new(8i64 << 20)
        );

        // min/max (MessageId.h:70-75)
        assert_eq!(MessageId::min().get(), 1);
        assert_eq!(MessageId::max().get(), (i32::MAX as i64) << 20);
        assert!(MessageId::max().is_valid());
        // max() is already a rounded-up server id, so next == max
        assert_eq!(
            MessageId::max().get_next_server_message_id(),
            MessageId::max()
        );

        // sponsored: above max() with the local type bits — the layout is
        // |1..1(31)|local_id(17)|0|10|, so an occupied local part pushes the
        // id above max()
        let sponsored = MessageId::new(((i32::MAX as i64) << 20) | (1 << 3) | 2);
        assert!(sponsored.is_valid_sponsored());
        // bare max() has type bits 0 — a server id, not sponsored
        assert!(!MessageId::new((i32::MAX as i64) << 20).is_valid_sponsored());
    }

    #[test]
    fn message_id_store_parse_round_trips_the_raw_id() {
        let message_id =
            MessageId::from_scheduled(ScheduledServerMessageId::new(13), (1 << 30) + 5, false);
        let mut out = Vec::new();
        message_id.store(&mut out);
        assert_eq!(out.len(), 8);
        let mut parser = Parser::new(&out);
        let parsed = MessageId::parse_from(&mut parser).expect("parses");
        parser.fetch_end().expect("fully consumed");
        assert_eq!(parsed, message_id);
    }
}
