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

use crate::global::Global;
use crate::misc::strip_empty_characters;
use crate::store_parse::{Parser, store_i32, store_string, store_u64, version};
use crate::td_api;
use tdmtproto::telegram_api;

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
}
