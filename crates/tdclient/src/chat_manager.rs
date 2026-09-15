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

/// `td::MessageId` (MessageId.h) — opaque stand-in, see the module docs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MessageId(i64);

impl MessageId {
    pub fn new(id: i64) -> MessageId {
        MessageId(id)
    }

    pub fn get(self) -> i64 {
        self.0
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
