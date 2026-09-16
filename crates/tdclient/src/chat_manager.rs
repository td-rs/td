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

use crate::emoji_status::{CustomEmojiId, EmojiStatus};
use crate::global::Global;
use crate::misc::strip_empty_characters;
use crate::peer_color::AccentColorId;
use crate::photo::{DialogPhoto, Photo};
use crate::restriction_reason::{
    RestrictionReason, get_restriction_reasons_from_legacy, parse_restriction_reasons,
    store_restriction_reasons,
};
use crate::store_parse::{
    Parser, store_f64, store_flags, store_i32, store_i64, store_string, store_u32, store_u64,
    version,
};
use crate::td_api;
use crate::user_manager::{
    CommunityId, FileId, SecretChatId, StoryId, TimeCtx, UserEnv, UserId, UserManager,
};
use crate::usernames::Usernames;
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

    /// `MessageId::is_valid` — the C++ classifies server/local/scheduled
    /// ids; the stand-in accepts any non-zero id (all ids stored today are
    /// server ids, whose encoding is always non-zero).
    pub fn is_valid(self) -> bool {
        self.0 != 0
    }

    /// The `MessageId` header `store` (MessageId.h:193-200) — the raw id
    /// both ways, no version gate.
    pub fn store(&self, out: &mut Vec<u8>) {
        store_i64(out, self.0);
    }

    pub fn parse_impl(parser: &mut Parser<'_>) -> Result<MessageId, &'static str> {
        Ok(MessageId(parser.i64_()?))
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
            telegram_api::DialogPeer::dialogPeer(peer) => DialogId(Self::get_peer_id(&peer.peer)),
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
                        participant.admin_rights.anonymous,
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
                        &participant.admin_rights,
                        participant.rank,
                        channel_type,
                    ),
                }
            }
            telegram_api::ChannelParticipant::channelParticipantLeft(participant) => {
                DialogParticipant {
                    dialog_id_: DialogId::from_peer(&participant.peer),
                    inviter_user_id_: UserId::default(),
                    joined_date_: 0,
                    status_: DialogParticipantStatus::left(),
                }
            }
            telegram_api::ChannelParticipant::channelParticipantBanned(participant) => {
                DialogParticipant {
                    dialog_id_: DialogId::from_peer(&participant.peer),
                    inviter_user_id_: UserId::new(participant.kicked_by),
                    joined_date_: participant.date,
                    status_: DialogParticipantStatus::from_channel_banned_rights(
                        !participant.left,
                        participant.banned_rights,
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
            pricing_: StarSubscriptionPricing::from_telegram(Some(
                exported_invite.subscription_pricing,
            )),
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

        if invite_link.is_permanent_
            && (!invite_link.title_.is_empty()
                || invite_link.expire_date_ > 0
                || invite_link.usage_limit_ > 0
                || invite_link.edit_date_ > 0
                || invite_link.request_count_ > 0)
        {
            tdutils::log_error!("Receive wrong permanent {full_source}");
            invite_link.title_.clear();
            invite_link.expire_date_ = 0;
            invite_link.usage_limit_ = 0;
            invite_link.edit_date_ = 0;
            invite_link.request_count_ = 0;
        }
        if invite_link.creates_join_request_ && invite_link.usage_limit_ > 0 {
            tdutils::log_error!("Receive wrong permanent {full_source}");
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
            // field is a plain struct, so an empty pricing serializes with
            // its zero fields instead of null.
            subscription_pricing: self
                .pricing_
                .get_star_subscription_pricing_object()
                .unwrap_or_default(),
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
    /// `DialogLocation::empty` (DialogLocation.cpp).
    pub fn empty(&self) -> bool {
        self.location_.is_empty() && self.address_.is_empty()
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
    description_: String,
}

impl BotVerification {
    /// `BotVerification(telegram_api::object_ptr<botVerification> &&)`
    /// (BotVerification.cpp:18-25).
    pub fn from_telegram(bot_verification: telegram_api::BotVerification) -> Self {
        BotVerification {
            bot_user_id_: UserId::new(bot_verification.bot_id),
            icon_: CustomEmojiId::new(bot_verification.icon),
            description_: bot_verification.description,
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

    /// `store` (BotVerification.hpp:17-23) — the (empty) flag block, then
    /// the bot user id, the icon and the description.
    pub fn store(&self, out: &mut Vec<u8>) {
        store_flags(out, []);
        store_i64(out, self.bot_user_id_.get());
        store_i64(out, self.icon_.get());
        store_string(out, &self.description_);
    }

    /// `parse` (BotVerification.hpp:25-31).
    pub fn parse_impl(parser: &mut Parser<'_>) -> Result<Self, &'static str> {
        parser.parse_flags::<0>()?;
        let bot_user_id_ = parse_user_id(parser)?;
        let icon_ = CustomEmojiId::new(parser.i64_()?);
        let description_ = parser.string()?;
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
            channel_full.migrated_from_max_message_id = MessageId::parse_impl(parser)?;
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

        let wrapped =
            telegram_api::DialogPeer::dialogPeer(telegram_api::dialogPeer { peer: peer_channel });
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
                    admin_rights: telegram_api::ChatAdminRights {
                        anonymous: true,
                        change_info: true,
                        invite_users: true,
                        pin_messages: true,
                        ..Default::default()
                    },
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
                    admin_rights: telegram_api::ChatAdminRights {
                        change_info: true,
                        invite_users: true,
                        ..Default::default()
                    },
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
                    peer: telegram_api::Peer::peerChannel(telegram_api::peerChannel {
                        channel_id: 1000000002,
                    }),
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
                    peer: telegram_api::Peer::peerUser(telegram_api::peerUser {
                        user_id: 3000000022,
                    }),
                    kicked_by: 3000000023,
                    date: 1700001006,
                    banned_rights: telegram_api::ChatBannedRights {
                        send_messages: true,
                        until_date: 1700003600,
                        ..Default::default()
                    },
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
                    peer: telegram_api::Peer::peerChat(telegram_api::peerChat {
                        chat_id: 1234567890,
                    }),
                    kicked_by: 0,
                    date: 1700001007,
                    banned_rights: telegram_api::ChatBannedRights {
                        view_messages: true,
                        ..Default::default()
                    },
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
            subscription_pricing: telegram_api::StarsSubscriptionPricing::default(),
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

        // a permanent link may carry no extra parameters
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
        assert_eq!(wrong_permanent.request_count_, 0);

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
                    subscription_pricing: telegram_api::StarsSubscriptionPricing {
                        period: 6,
                        amount: 50,
                    },
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
        assert_eq!(object.subscription_pricing.period, 6);
        assert_eq!(object.subscription_pricing.star_count, 50);
        assert_eq!(object.member_limit, 0);
        assert_eq!(object.member_count, 12);
        assert_eq!(object.expired_member_count, 0);
        // the permanent sanitizer cleared the pending-request count
        assert_eq!(object.pending_join_request_count, 0);
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
                description: "stamp".to_owned(),
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
            description: "stamp".to_owned(),
        });
        assert!(verification.is_valid());
        assert!(
            !BotVerification::from_telegram(telegram_api::BotVerification {
                bot_id: 0,
                icon: 99,
                description: String::new(),
            })
            .is_valid()
        );
        // an invalid record is dropped by the get_bot_verification gate
        assert!(
            BotVerification::get_bot_verification(Some(telegram_api::BotVerification {
                bot_id: 0,
                icon: 0,
                description: String::new(),
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
}
