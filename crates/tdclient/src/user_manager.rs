//! Port of `td/telegram/UserManager.{h,cpp}` (Track I, M.3): the user /
//! secret-chat cache with its change-tracking machine, the byte-exact
//! database serialization, `on_get_user`, the `updateUser`/`updateUserStatus`
//! machinery, the td_api object conversion, `get_current_state` and the
//! special-user table.
//!
//! Structure and formats are 1:1 with the C++ (`User::store`/`parse`
//! cpp:1808-2175, `UserFull` cpp:2178-2610, `SecretChat` cpp:2613-2666,
//! `on_get_user` cpp:3127-3452, `update_user` cpp:9872-10082, object
//! conversion cpp:10227-10566). Divergences, each arriving with its
//! Wave-1 step:
//! - profile photos are tracked in memory but not persisted — the
//!   `DialogPhoto`/`ProfilePhoto` byte format delegates the two `FileId`s
//!   to `FileManager::store_file`, which lands with the Files step
//!   (`has_photo` is forced `false` in stores; a value with the photo
//!   flag set fails to parse, exactly like a C++ parse failure);
//! - `UserFull` carries the scalar/bool fields, `birthdate`,
//!   `personal_channel_id` and `linked_community_id`; the bot-info,
//!   business, star-rating, note and saved-music fields arrive with their
//!   subsystems (a value with those fields set fails to parse);
//! - the users binlog mirror (`UserLogEvent` + `on_binlog_user_event`) is
//!   in: unsaved users log to the binlog (`HandlerType::Users`), the
//!   sqlite save retires the event, and reopen replays it; the C++'s
//!   async-save window collapses onto the synchronous "common" kv write;
//! - cross-manager notifications (MessagesManager dialog title/photo
//!   updates, StoryManager, ReactionManager, ContactsManager hints,
//!   OnlineManager, ConfigManager) are documented no-ops in
//!   `update_user` until those managers land;
//! - the network-backed paths (`reload_user`, `load_user_full` queries,
//!   `QueryMerger`/`QueryCombiner`) stay with the net step;
//! - the current codegen cannot express nullable td_api fields (the
//!   nullability modeling lands with the S76 codegen merge), so
//!   `get_user_object`/`get_user_full_info_object` emit the C++-empty
//!   payloads (empty `usernames`, `userStatusEmpty`, the empty photo,
//!   `activeStoryStateRead`, ...) where the C++ emits `null`.

use std::collections::{BTreeMap, BTreeSet};

use crate::birthdate::Birthdate;
use crate::emoji_status::{CustomEmojiId, EmojiStatus};
use crate::global::Global;
use crate::option_manager::OptionManager;
use crate::peer_color::{AccentColorId, PeerColor, PeerColorCollectible};
use crate::restriction_reason::{
    RestrictionOptions, RestrictionReason, get_restriction_info_object, parse_restriction_reasons,
    store_restriction_reasons,
};
use crate::store_parse::{self, Parser};
use crate::td_api;
use crate::usernames::Usernames;
use tddb::binlog_key_value::BinlogKeyValue;
use tdmtproto::telegram_api;

/// `User::CACHE_VERSION` (UserManager.h:633).
const USER_CACHE_VERSION: u32 = 4;
/// `USER_FULL_EXPIRE_TIME` (UserManager.h:876).
const USER_FULL_EXPIRE_TIME: f64 = 60.0;
/// `MAX_ACTIVE_STORY_ID_RELOAD_TIME` (UserManager.h:874).
const MAX_ACTIVE_STORY_ID_RELOAD_TIME: f64 = 3600.0;
/// `SecretChatLayer::Default` (SecretChatId.h).
const SECRET_CHAT_DEFAULT_LAYER: i32 = 73;

// telegram_api::user flag masks (telegram_api.h, the `user` class:
// ACCESS_HASH_MASK = 1, FIRST_NAME_MASK = 2, ..., PHONE_MASK = 16,
// BOT_INFO_VERSION_MASK = 1 << 14, BOT_INLINE_PLACEHOLDER_MASK = 1 << 19).
const USER_ACCESS_HASH_MASK: i32 = 1;
const USER_PHONE_MASK: i32 = 1 << 4;
const USER_BOT_INLINE_PLACEHOLDER_MASK: i32 = 1 << 19;
// telegram_api::user flags2 mask: stories_max_id:flags2.5.
const USER_STORIES_MAX_ID_MASK: i32 = 1 << 5;

/// `StarManager::get_star_count(amount, false)` (StarManager.cpp).
fn get_star_count(amount: i64) -> i64 {
    let max_amount = 1i64 << 51;
    if amount < 0 {
        tdutils::log_error!("Receive Telegram Star amount = {amount}");
        return 0;
    }
    if amount > max_amount {
        tdutils::log_error!("Receive Telegram Star amount = {amount}");
        return max_amount;
    }
    amount
}

/// `td::UserId` (UserId.h): 0 < id ≤ 2^40 - 1.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UserId(i64);

impl UserId {
    pub const MAX_USER_ID: i64 = (1i64 << 40) - 1;

    pub fn new(id: i64) -> UserId {
        UserId(id)
    }

    pub fn is_valid(self) -> bool {
        0 < self.0 && self.0 <= Self::MAX_USER_ID
    }

    pub fn get(self) -> i64 {
        self.0
    }
}

/// `td::SecretChatId` (SecretChatId.h): 0 < id.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SecretChatId(i32);

impl SecretChatId {
    pub fn new(id: i32) -> SecretChatId {
        SecretChatId(id)
    }

    pub fn is_valid(self) -> bool {
        self.0 > 0
    }

    pub fn get(self) -> i32 {
        self.0
    }
}

/// `td::FileId` — the file-manager handle. The store/parse of files lands
/// with the Files step; the id is threaded through the photo structures
/// so the shapes are already in place.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FileId(i32);

impl FileId {
    pub fn is_valid(self) -> bool {
        self.0 > 0
    }
}

/// `td::StoryId` (StoryId.h).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct StoryId(i32);

impl StoryId {
    pub const MAX_SERVER_STORY_ID: i32 = 1_999_999_999;

    pub fn new(id: i32) -> StoryId {
        StoryId(id)
    }

    pub fn get(self) -> i32 {
        self.0
    }

    pub fn is_valid(self) -> bool {
        self.0 > 0
    }

    pub fn is_server(self) -> bool {
        self.0 > 0 && self.0 <= Self::MAX_SERVER_STORY_ID
    }
}

/// `td::CommunityId` (CommunityId.h).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CommunityId(i64);

impl CommunityId {
    pub fn new(id: i64) -> CommunityId {
        CommunityId(id)
    }

    pub fn is_valid(self) -> bool {
        self.0 > 0
    }

    pub fn get(self) -> i64 {
        self.0
    }
}

/// `td::ProfilePhoto` (DialogPhoto.h:31-34) — the DialogPhoto base fields
/// plus the photo id. The `FileId`s stay placeholders until the Files step
/// (the module doc lists the consequences).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProfilePhoto {
    small_file_id: FileId,
    big_file_id: FileId,
    minithumbnail: String,
    has_animation: bool,
    is_personal: bool,
    id: i64,
}

impl ProfilePhoto {
    pub fn is_empty(&self) -> bool {
        self.id == 0
    }

    /// `DialogPhoto.hpp store/parse` with the two file ids forced empty —
    /// the FileManager serialization is the Files step.
    pub fn store(&self, out: &mut Vec<u8>) {
        use store_parse::*;
        let has_file_ids = false; // DIVERGENCE(Files step)
        let has_minithumbnail = !self.minithumbnail.is_empty();
        store_flags(
            out,
            [
                has_file_ids,
                self.has_animation,
                has_minithumbnail,
                self.is_personal,
            ],
        );
        if has_file_ids {
            store_i32(out, self.small_file_id.0);
            store_i32(out, self.big_file_id.0);
        }
        if has_minithumbnail {
            store_string(out, &self.minithumbnail);
        }
        store_i64(out, self.id);
    }
}

/// `get_profile_photo` (DialogPhoto.cpp:296-322) without the file-manager
/// registration: the telegram photo id, animation/personal flags and the
/// stripped thumbnail are kept; the file ids stay invalid.
fn get_profile_photo(profile_photo: telegram_api::UserProfilePhoto) -> ProfilePhoto {
    let mut result = ProfilePhoto::default();
    let telegram_api::UserProfilePhoto::userProfilePhoto(profile_photo) = profile_photo else {
        return result;
    };
    if profile_photo.photo_id == 0 || profile_photo.photo_id == -2 {
        tdutils::log_error!("Receive a profile photo without identifier");
        return result;
    }
    result.has_animation = profile_photo.has_video;
    result.is_personal = profile_photo.personal;
    result.id = profile_photo.photo_id;
    result.minithumbnail = String::from_utf8_lossy(&profile_photo.stripped_thumb).into_owned();
    result
}

/// `get_profile_photo_id` (DialogPhoto.cpp) — used by the deferred
/// `pending_user_photos_`/user-full branches.
#[allow(dead_code)]
fn get_profile_photo_id(profile_photo: &telegram_api::UserProfilePhoto) -> i64 {
    match profile_photo {
        telegram_api::UserProfilePhoto::userProfilePhoto(photo) => photo.photo_id,
        telegram_api::UserProfilePhoto::userProfilePhotoEmpty(_) => 0,
    }
}

/// `need_update_dialog_photo` (DialogPhoto.cpp).
fn need_update_dialog_photo(from: &ProfilePhoto, to: &ProfilePhoto) -> bool {
    from.small_file_id != to.small_file_id
        || from.big_file_id != to.big_file_id
        || from.has_animation != to.has_animation
        || from.is_personal != to.is_personal
}

/// `need_update_profile_photo` (DialogPhoto.cpp).
fn need_update_profile_photo(from: &ProfilePhoto, to: &ProfilePhoto) -> bool {
    from.id != to.id || need_update_dialog_photo(from, to)
}

/// `need_update_dialog_photo_minithumbnail`.
fn need_update_dialog_photo_minithumbnail(from: &str, to: &str) -> bool {
    from != to
}

/// `td::SecretChatState` (SecretChatId.h:17).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SecretChatState {
    #[default]
    Unknown = -1,
    Waiting = 0,
    Active = 1,
    Closed = 2,
}

/// `clean_phone_number` (misc.h) — keep digits and '+' only.
fn clean_phone_number(phone_number: String) -> String {
    phone_number
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == '+')
        .collect()
}

/// `td::UserManager::User` (UserManager.h:595-704) with the full
/// change-tracking flag set and the C++ field defaults.
#[derive(Debug)]
pub struct User {
    pub first_name: String,
    pub last_name: String,
    pub usernames: Usernames,
    pub phone_number: String,
    pub access_hash: i64,
    pub emoji_status: Option<EmojiStatus>,
    pub last_sent_emoji_status: Option<EmojiStatus>,

    pub photo: ProfilePhoto,

    pub restriction_reasons: Vec<RestrictionReason>,
    pub inline_query_placeholder: String,
    pub bot_active_users: i32,
    pub bot_info_version: i32,

    pub bot_verification_icon: CustomEmojiId,

    pub accent_color_id: AccentColorId,
    pub background_custom_emoji_id: CustomEmojiId,
    pub peer_color_collectible: Option<PeerColorCollectible>,
    pub profile_accent_color_id: AccentColorId,
    pub profile_background_custom_emoji_id: CustomEmojiId,
    pub linked_community_id: CommunityId,

    pub was_online: i32,
    pub local_was_online: i32,

    pub max_active_story_id_next_reload_time: f64,
    pub max_active_story_id: StoryId,
    pub max_read_story_id: StoryId,

    pub paid_message_star_count: i64,

    pub language_code: String,

    pub cache_version: u32,

    pub is_min_access_hash: bool,
    pub is_received: bool,
    pub is_verified: bool,
    pub is_premium: bool,
    pub is_support: bool,
    pub is_deleted: bool,
    pub is_bot: bool,
    pub can_join_groups: bool,
    pub can_read_all_group_messages: bool,
    pub can_be_edited_bot: bool,
    pub has_main_app: bool,
    pub has_bot_forum_view: bool,
    pub can_bot_create_topics: bool,
    pub can_manage_bots: bool,
    pub is_inline_bot: bool,
    pub is_guestchat_bot: bool,
    pub is_business_bot: bool,
    pub is_guard_bot: bool,
    pub need_location_bot: bool,
    pub is_scam: bool,
    pub is_fake: bool,
    pub is_contact: bool,
    pub is_mutual_contact: bool,
    pub is_close_friend: bool,
    pub need_apply_min_photo: bool,
    pub can_be_added_to_attach_menu: bool,
    pub attach_menu_enabled: bool,
    pub stories_hidden: bool,
    pub contact_require_premium: bool,
    pub has_live_story: bool,

    pub is_photo_inited: bool,

    pub is_repaired: bool,

    pub is_name_changed: bool,
    pub is_username_changed: bool,
    pub is_photo_changed: bool,
    pub is_accent_color_changed: bool,
    pub is_phone_number_changed: bool,
    pub is_emoji_status_changed: bool,
    pub is_is_contact_changed: bool,
    pub is_is_mutual_contact_changed: bool,
    pub is_is_deleted_changed: bool,
    pub is_is_premium_changed: bool,
    pub is_stories_hidden_changed: bool,
    pub is_has_bot_forum_view_changed: bool,
    pub is_full_info_changed: bool,
    pub is_being_updated: bool,
    pub is_changed: bool,
    pub need_save_to_database: bool,
    pub is_status_changed: bool,
    pub is_online_status_changed: bool,
    pub is_update_user_sent: bool,

    pub is_saved: bool,
    pub is_being_saved: bool,
    pub is_status_saved: bool,

    /// The binlog id of the outstanding `UserLogEvent` mirror (0 = none) —
    /// the WAL covering the window until the sqlite row is flushed.
    pub log_event_id: u64,

    pub is_received_from_server: bool,
}

impl Default for User {
    fn default() -> Self {
        User {
            first_name: String::new(),
            last_name: String::new(),
            usernames: Usernames::default(),
            phone_number: String::new(),
            access_hash: -1,
            emoji_status: None,
            last_sent_emoji_status: None,
            photo: ProfilePhoto::default(),
            restriction_reasons: Vec::new(),
            inline_query_placeholder: String::new(),
            bot_active_users: 0,
            bot_info_version: -1,
            bot_verification_icon: CustomEmojiId::default(),
            accent_color_id: AccentColorId::default(),
            background_custom_emoji_id: CustomEmojiId::default(),
            peer_color_collectible: None,
            profile_accent_color_id: AccentColorId::default(),
            profile_background_custom_emoji_id: CustomEmojiId::default(),
            linked_community_id: CommunityId::default(),
            was_online: 0,
            local_was_online: 0,
            max_active_story_id_next_reload_time: 0.0,
            max_active_story_id: StoryId::default(),
            max_read_story_id: StoryId::default(),
            paid_message_star_count: 0,
            language_code: String::new(),
            cache_version: 0,
            is_min_access_hash: true,
            is_received: false,
            is_verified: false,
            is_premium: false,
            is_support: false,
            is_deleted: true,
            is_bot: true,
            can_join_groups: true,
            can_read_all_group_messages: true,
            can_be_edited_bot: false,
            has_main_app: false,
            has_bot_forum_view: false,
            can_bot_create_topics: false,
            can_manage_bots: false,
            is_inline_bot: false,
            is_guestchat_bot: false,
            is_business_bot: false,
            is_guard_bot: false,
            need_location_bot: false,
            is_scam: false,
            is_fake: false,
            is_contact: false,
            is_mutual_contact: false,
            is_close_friend: false,
            need_apply_min_photo: false,
            can_be_added_to_attach_menu: false,
            attach_menu_enabled: false,
            stories_hidden: false,
            contact_require_premium: false,
            has_live_story: false,
            is_photo_inited: false,
            is_repaired: false,
            is_name_changed: true,
            is_username_changed: true,
            is_photo_changed: true,
            is_accent_color_changed: true,
            is_phone_number_changed: true,
            is_emoji_status_changed: true,
            is_is_contact_changed: true,
            is_is_mutual_contact_changed: true,
            is_is_deleted_changed: true,
            is_is_premium_changed: true,
            is_stories_hidden_changed: true,
            is_has_bot_forum_view_changed: true,
            is_full_info_changed: false,
            is_being_updated: false,
            is_changed: true,
            need_save_to_database: true,
            is_status_changed: true,
            is_online_status_changed: true,
            is_update_user_sent: false,
            is_saved: false,
            is_being_saved: false,
            is_status_saved: false,
            log_event_id: 0,
            is_received_from_server: false,
        }
    }
}

/// `UserManager::UserLogEvent` (UserManager.cpp:2668-2690), store side: the
/// log-event version prefix, then the user id, then the `User` store —
/// the same framing `get_log_event_storer` gives the C++ binlog writer.
fn store_user_log_event(out: &mut Vec<u8>, user_id: UserId, u: &User, ctx: &TimeCtx) {
    store_parse::store_log_event_version(out);
    store_parse::store_i64(out, user_id.get());
    u.store(out, ctx);
}

/// `UserManager::UserLogEvent`, parse side (`log_event_parse` over the
/// binlog event data).
fn parse_user_log_event(input: &[u8], ctx: &TimeCtx) -> Result<(UserId, User), &'static str> {
    let mut parser = Parser::new_log_event(input)?;
    let user_id = UserId::new(parser.i64_()?);
    let user = User::parse_impl(&mut parser, ctx)?;
    parser.fetch_end()?;
    Ok((user_id, user))
}

/// The `store_time` clock bundle (LogEventHelper.h:32-56): the writer
/// needs `Time::now` and `get_global_server_time()`, the reader the
/// server time to translate the stored `time_left` back.
#[derive(Clone, Copy)]
pub struct TimeCtx {
    pub now: f64,
    pub server_time: f64,
}

impl TimeCtx {
    pub fn from_global(global: &Global) -> TimeCtx {
        TimeCtx {
            now: tdutils::time::now(),
            server_time: global.server_time(),
        }
    }

    fn store_time(out: &mut Vec<u8>, time_at: f64, ctx: &TimeCtx) {
        if time_at == 0.0 {
            store_parse::store_f64(out, -1.0);
        } else {
            let time_left = (time_at - ctx.now).max(0.0);
            store_parse::store_f64(out, time_left);
            store_parse::store_f64(out, ctx.server_time);
        }
    }

    fn parse_time(parser: &mut Parser, ctx: &TimeCtx) -> Result<f64, &'static str> {
        let time_left = parser.f64_()?;
        if time_left < -0.1 {
            return Ok(0.0);
        }
        let old_server_time = parser.f64_()?;
        let passed_server_time = (ctx.server_time - old_server_time).max(0.0);
        let time_left = (time_left - passed_server_time).max(0.0);
        Ok(ctx.now + time_left)
    }
}

impl User {
    pub fn new() -> User {
        User::default()
    }

    /// `User::store` (UserManager.cpp:1808-1962) with the photo
    /// persistence deferred (module docs).
    pub fn store(&self, out: &mut Vec<u8>, ctx: &TimeCtx) {
        use store_parse::*;
        let has_last_name = !self.last_name.is_empty();
        let legacy_has_username = false;
        let has_photo = false; // DIVERGENCE(Files step)
        let has_language_code = !self.language_code.is_empty();
        let have_access_hash = self.access_hash != -1;
        let has_cache_version = self.cache_version != 0;
        let has_is_contact = true;
        let has_restriction_reasons = !self.restriction_reasons.is_empty();
        let has_emoji_status = self.emoji_status.is_some();
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
        let has_bot_active_users = self.bot_active_users != 0;
        let has_bot_verification_icon = self.bot_verification_icon.is_valid();
        let has_paid_message_star_count = self.paid_message_star_count != 0;
        let has_peer_color_collectible = self.peer_color_collectible.is_some();
        let has_linked_community_id = self.linked_community_id.is_valid();
        store_flags(
            out,
            [
                self.is_received,
                self.is_verified,
                self.is_deleted,
                self.is_bot,
                self.can_join_groups,
                self.can_read_all_group_messages,
                self.is_inline_bot,
                self.need_location_bot,
                has_last_name,
                legacy_has_username,
                has_photo,
                false, // legacy is_restricted
                has_language_code,
                have_access_hash,
                self.is_support,
                self.is_min_access_hash,
                self.is_scam,
                has_cache_version,
                has_is_contact,
                self.is_contact,
                self.is_mutual_contact,
                has_restriction_reasons,
                self.need_apply_min_photo,
                self.is_fake,
                self.can_be_added_to_attach_menu,
                self.is_premium,
                self.attach_menu_enabled,
                has_emoji_status,
                has_usernames,
                self.can_be_edited_bot,
            ],
        );
        if has_flags2 {
            store_flags(
                out,
                [
                    self.is_close_friend,
                    self.stories_hidden,
                    false, // legacy has_stories
                    has_max_active_story_id,
                    has_max_read_story_id,
                    has_max_active_story_id_next_reload_time,
                    has_accent_color_id,
                    has_background_custom_emoji_id,
                    has_profile_accent_color_id,
                    has_profile_background_custom_emoji_id,
                    self.contact_require_premium,
                    self.is_business_bot,
                    has_bot_active_users,
                    self.has_main_app,
                    has_bot_verification_icon,
                    has_paid_message_star_count,
                    has_peer_color_collectible,
                    self.has_bot_forum_view,
                    self.has_live_story,
                    self.can_bot_create_topics,
                    self.can_manage_bots,
                    self.is_guestchat_bot,
                    self.is_guard_bot,
                    has_linked_community_id,
                ],
            );
        }
        store_string(out, &self.first_name);
        if has_last_name {
            store_string(out, &self.last_name);
        }
        store_string(out, &self.phone_number);
        if have_access_hash {
            store_i64(out, self.access_hash);
        }
        // if (has_photo) { store(photo, storer); } — Files step
        store_i32(out, self.was_online);
        if has_restriction_reasons {
            store_restriction_reasons(out, &self.restriction_reasons);
        }
        if self.is_inline_bot {
            store_string(out, &self.inline_query_placeholder);
        }
        if self.is_bot {
            store_i32(out, self.bot_info_version);
        }
        if has_language_code {
            store_string(out, &self.language_code);
        }
        if has_cache_version {
            store_u32(out, self.cache_version);
        }
        if has_emoji_status {
            self.emoji_status.as_ref().unwrap().store(out);
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
        if has_bot_active_users {
            store_i32(out, self.bot_active_users);
        }
        if has_bot_verification_icon {
            store_i64(out, self.bot_verification_icon.get());
        }
        if has_paid_message_star_count {
            store_i64(out, self.paid_message_star_count);
        }
        if has_peer_color_collectible {
            self.peer_color_collectible.as_ref().unwrap().store(out);
        }
        if has_linked_community_id {
            store_i64(out, self.linked_community_id.get());
        }
    }

    /// `log_event_store(User)` — the version-prefixed database value.
    pub fn store_value(&self, out: &mut Vec<u8>, ctx: &TimeCtx) {
        store_parse::store_log_event_version(out);
        self.store(out, ctx);
    }

    /// `log_event_parse(User)` — the version-prefixed database value.
    pub fn parse_value(input: &[u8], ctx: &TimeCtx) -> Result<User, &'static str> {
        let mut parser = Parser::new_log_event(input)?;
        let user = User::parse_impl(&mut parser, ctx)?;
        parser.fetch_end()?;
        Ok(user)
    }

    fn parse_impl(parser: &mut Parser, ctx: &TimeCtx) -> Result<User, &'static str> {
        use store_parse::version;
        let [
            is_received,
            is_verified,
            is_deleted,
            is_bot,
            can_join_groups,
            can_read_all_group_messages,
            is_inline_bot,
            need_location_bot,
            has_last_name,
            legacy_has_username,
            has_photo,
            legacy_is_restricted,
            has_language_code,
            have_access_hash,
            is_support,
            is_min_access_hash,
            is_scam,
            has_cache_version,
            has_is_contact,
            is_contact,
            is_mutual_contact,
            has_restriction_reasons,
            need_apply_min_photo,
            is_fake,
            can_be_added_to_attach_menu,
            is_premium,
            attach_menu_enabled,
            has_emoji_status,
            has_usernames,
            can_be_edited_bot,
        ] = parser.parse_flags()?;
        if has_photo {
            return Err("User photo needs the FileManager format (Files step)");
        }
        let has_flags2 = parser.version() >= version::ADD_USER_FLAGS2;
        let mut u = User::new();
        u.is_received = is_received;
        u.is_verified = is_verified;
        u.is_deleted = is_deleted;
        u.is_bot = is_bot;
        u.can_join_groups = can_join_groups;
        u.can_read_all_group_messages = can_read_all_group_messages;
        u.is_inline_bot = is_inline_bot;
        u.need_location_bot = need_location_bot;
        u.is_support = is_support;
        u.is_min_access_hash = is_min_access_hash;
        u.is_scam = is_scam;
        u.is_contact = is_contact;
        u.is_mutual_contact = is_mutual_contact;
        u.need_apply_min_photo = need_apply_min_photo;
        u.is_fake = is_fake;
        u.can_be_added_to_attach_menu = can_be_added_to_attach_menu;
        u.is_premium = is_premium;
        u.attach_menu_enabled = attach_menu_enabled;
        u.can_be_edited_bot = can_be_edited_bot;

        let [
            is_close_friend,
            stories_hidden,
            legacy_has_stories,
            has_max_active_story_id,
            has_max_read_story_id,
            has_max_active_story_id_next_reload_time,
            has_accent_color_id,
            has_background_custom_emoji_id,
            has_profile_accent_color_id,
            has_profile_background_custom_emoji_id,
            contact_require_premium,
            is_business_bot,
            has_bot_active_users,
            has_main_app,
            has_bot_verification_icon,
            has_paid_message_star_count,
            has_peer_color_collectible,
            has_bot_forum_view,
            has_live_story,
            can_bot_create_topics,
            can_manage_bots,
            is_guestchat_bot,
            is_guard_bot,
            has_linked_community_id,
        ] = if has_flags2 {
            parser.parse_flags()?
        } else {
            [false; 24]
        };
        if legacy_has_stories {
            return Err("legacy user stories need the story subsystem");
        }
        u.is_close_friend = is_close_friend;
        u.stories_hidden = stories_hidden;
        u.contact_require_premium = contact_require_premium;
        u.is_business_bot = is_business_bot;
        u.has_main_app = has_main_app;
        u.has_bot_forum_view = has_bot_forum_view;
        u.has_live_story = has_live_story;
        u.can_bot_create_topics = can_bot_create_topics;
        u.can_manage_bots = can_manage_bots;
        u.is_guestchat_bot = is_guestchat_bot;
        u.is_guard_bot = is_guard_bot;

        u.first_name = parser.string()?;
        if has_last_name {
            u.last_name = parser.string()?;
        }
        if legacy_has_username {
            if has_usernames {
                return Err("both legacy and new usernames stored");
            }
            let username = parser.string()?;
            u.usernames = Usernames::from_telegram(username, Vec::new());
        }
        u.phone_number = parser.string()?;
        let mut have_access_hash = have_access_hash;
        if parser.version() < version::FIX_MIN_USERS {
            have_access_hash = is_received;
        }
        if have_access_hash {
            u.access_hash = parser.i64_()?;
        } else {
            u.is_min_access_hash = true;
        }
        // if (has_photo) { parse(photo, parser); } — rejected above
        if !has_is_contact {
            // enum class LinkState : uint8 { Unknown, None, KnowsPhoneNumber, Contact };
            let link_state_inbound = parser.u32_()?;
            let link_state_outbound = parser.u32_()?;
            u.is_contact = link_state_outbound == 3;
            u.is_mutual_contact = u.is_contact && link_state_inbound == 3;
            u.is_close_friend = false;
        }
        u.was_online = parser.i32_()?;
        if legacy_is_restricted {
            let restriction_reason = parser.string()?;
            u.restriction_reasons =
                crate::restriction_reason::get_restriction_reasons_from_legacy(&restriction_reason);
        } else if has_restriction_reasons {
            u.restriction_reasons = parse_restriction_reasons(parser)?;
        }
        if is_inline_bot {
            u.inline_query_placeholder = parser.string()?;
        }
        if is_bot {
            u.bot_info_version = parser.i32_()?;
        }
        if has_language_code {
            u.language_code = parser.string()?;
        }
        if has_cache_version {
            u.cache_version = parser.u32_()?;
        }
        if has_emoji_status {
            u.emoji_status = Some(EmojiStatus::parse_from(parser)?);
        }
        if has_usernames {
            if legacy_has_username {
                return Err("both legacy and new usernames stored");
            }
            u.usernames = Usernames::parse_tail(parser)?;
        }
        if has_max_active_story_id {
            u.max_active_story_id = StoryId::new(parser.i32_()?);
        }
        if has_max_read_story_id {
            u.max_read_story_id = StoryId::new(parser.i32_()?);
        }
        if has_max_active_story_id_next_reload_time {
            u.max_active_story_id_next_reload_time = TimeCtx::parse_time(parser, ctx)?;
        }
        if has_accent_color_id {
            u.accent_color_id = AccentColorId::new(parser.i32_()?);
        }
        if has_background_custom_emoji_id {
            u.background_custom_emoji_id = CustomEmojiId::new(parser.i64_()?);
        }
        if has_profile_accent_color_id {
            u.profile_accent_color_id = AccentColorId::new(parser.i32_()?);
        }
        if has_profile_background_custom_emoji_id {
            u.profile_background_custom_emoji_id = CustomEmojiId::new(parser.i64_()?);
        }
        if has_bot_active_users {
            u.bot_active_users = parser.i32_()?;
        }
        if has_bot_verification_icon {
            u.bot_verification_icon = CustomEmojiId::new(parser.i64_()?);
        }
        if has_paid_message_star_count {
            u.paid_message_star_count = parser.i64_()?;
        }
        if has_peer_color_collectible {
            u.peer_color_collectible = Some(PeerColorCollectible::parse_from(parser)?);
        }
        if has_linked_community_id {
            u.linked_community_id = CommunityId::new(parser.i64_()?);
        }

        if !tdutils::utf8::check_utf8(u.first_name.as_bytes()) {
            tdutils::log_error!("Have invalid first name \"{}\"", u.first_name);
            u.first_name.clear();
            u.cache_version = 0;
        }
        if !tdutils::utf8::check_utf8(u.last_name.as_bytes()) {
            tdutils::log_error!("Have invalid last name \"{}\"", u.last_name);
            u.last_name.clear();
            u.cache_version = 0;
        }

        u.phone_number = clean_phone_number(std::mem::take(&mut u.phone_number));
        if u.first_name.is_empty() && u.last_name.is_empty() {
            u.first_name = u.phone_number.clone();
        }
        if !u.is_contact && u.is_mutual_contact {
            tdutils::log_error!("Have invalid flag is_mutual_contact");
            u.is_mutual_contact = false;
            u.cache_version = 0;
        }
        if !u.is_contact && u.is_close_friend {
            tdutils::log_error!("Have invalid flag is_close_friend");
            u.is_close_friend = false;
            u.cache_version = 0;
        }
        Ok(u)
    }
}

/// `td::UserManager::UserFull` (UserManager.h:728-809) — the supported
/// subset (module docs); the store/parse keeps the C++ flag layout so the
/// supported fields stay byte-compatible.
#[derive(Debug)]
pub struct UserFull {
    pub photo: ProfilePhoto,
    pub fallback_photo: ProfilePhoto,
    pub personal_photo: ProfilePhoto,

    pub about: String,
    pub private_forward_name: String,

    pub gift_count: i32,
    pub common_chat_count: i32,
    pub birthdate: Birthdate,

    pub personal_channel_id: i64,
    pub linked_community_id: CommunityId,

    pub charge_paid_message_stars: i64,
    pub send_paid_message_stars: i64,

    pub is_blocked: bool,
    pub is_blocked_for_stories: bool,
    pub can_be_called: bool,
    pub supports_video_calls: bool,
    pub has_private_calls: bool,
    pub can_pin_messages: bool,
    pub need_phone_number_privacy_exception: bool,
    pub wallpaper_overridden: bool,
    pub noforwards_my_enabled: bool,
    pub noforwards_peer_enabled: bool,
    pub voice_messages_forbidden: bool,
    pub has_pinned_stories: bool,
    pub read_dates_private: bool,
    pub contact_require_premium: bool,
    pub sponsored_enabled: bool,
    pub has_preview_medias: bool,
    pub can_view_revenue: bool,
    pub can_manage_emoji_status: bool,
    pub unofficial_security_risk: bool,

    pub is_common_chat_count_changed: bool,
    pub is_has_protected_content_changed: bool,
    pub is_being_updated: bool,
    pub is_changed: bool,
    pub need_send_update: bool,
    pub need_save_to_database: bool,
    pub is_update_user_full_sent: bool,

    pub expires_at: f64,
}

impl Default for UserFull {
    fn default() -> Self {
        UserFull {
            photo: ProfilePhoto::default(),
            fallback_photo: ProfilePhoto::default(),
            personal_photo: ProfilePhoto::default(),
            about: String::new(),
            private_forward_name: String::new(),
            gift_count: 0,
            common_chat_count: 0,
            birthdate: Birthdate::default(),
            personal_channel_id: 0,
            linked_community_id: CommunityId::default(),
            charge_paid_message_stars: 0,
            send_paid_message_stars: 0,
            is_blocked: false,
            is_blocked_for_stories: false,
            can_be_called: false,
            supports_video_calls: false,
            has_private_calls: false,
            can_pin_messages: true,
            need_phone_number_privacy_exception: false,
            wallpaper_overridden: false,
            noforwards_my_enabled: false,
            noforwards_peer_enabled: false,
            voice_messages_forbidden: false,
            has_pinned_stories: false,
            read_dates_private: false,
            contact_require_premium: false,
            sponsored_enabled: false,
            has_preview_medias: false,
            can_view_revenue: false,
            can_manage_emoji_status: false,
            unofficial_security_risk: false,
            is_common_chat_count_changed: true,
            is_has_protected_content_changed: false,
            is_being_updated: false,
            is_changed: true,
            need_send_update: true,
            need_save_to_database: true,
            is_update_user_full_sent: false,
            expires_at: 0.0,
        }
    }
}

impl UserFull {
    pub fn new() -> UserFull {
        UserFull::default()
    }

    pub fn is_expired(&self, ctx: &TimeCtx) -> bool {
        self.expires_at < ctx.now
    }

    /// `UserFull::store` (UserManager.cpp:2178-2391), the supported subset.
    pub fn store(&self, out: &mut Vec<u8>, ctx: &TimeCtx) {
        use store_parse::*;
        let has_about = !self.about.is_empty();
        let has_photo = false; // DIVERGENCE(Files step)
        let has_description = false; // BotInfo subsystem
        let has_commands = false;
        let has_private_forward_name = !self.private_forward_name.is_empty();
        let has_group_administrator_rights = false;
        let has_broadcast_administrator_rights = false;
        let has_menu_button = false;
        let has_description_photo = false;
        let has_description_animation = false;
        let has_personal_photo = false; // DIVERGENCE(Files step)
        let has_fallback_photo = false; // DIVERGENCE(Files step)
        let has_business_info = false; // Business subsystem
        let has_birthdate = !self.birthdate.is_empty();
        let has_personal_channel_id = self.personal_channel_id != 0;
        let has_flags2 = true;
        let has_privacy_policy_url = false;
        let has_gift_count = self.gift_count != 0;
        let has_placeholder_path = false;
        let has_background_color = false;
        let has_background_dark_color = false;
        let has_header_color = false;
        let has_header_dark_color = false;
        let has_referral_program_info = false;
        let has_verifier_settings = false;
        let has_bot_verification = false;
        let has_charge_paid_message_stars = self.charge_paid_message_stars != 0;
        let has_send_paid_message_stars = self.send_paid_message_stars != 0;
        let has_gift_settings = false; // GiftSettings subsystem
        let has_star_rating = false; // StarRating subsystem
        let has_pending_star_rating = false;
        let has_main_profile_tab = false; // ProfileTab subsystem
        let has_first_saved_music_file_id = false; // Audios subsystem
        let has_note = false; // FormattedText subsystem
        let has_manager_bot_user_id = false;
        let has_linked_community_id = self.linked_community_id.is_valid();
        store_flags(
            out,
            [
                has_about,
                self.is_blocked,
                self.can_be_called,
                self.has_private_calls,
                self.can_pin_messages,
                self.need_phone_number_privacy_exception,
                has_photo,
                self.supports_video_calls,
                has_description,
                has_commands,
                has_private_forward_name,
                has_group_administrator_rights,
                has_broadcast_administrator_rights,
                has_menu_button,
                has_description_photo,
                has_description_animation,
                false, // legacy has_premium_gift_options
                self.voice_messages_forbidden,
                has_personal_photo,
                has_fallback_photo,
                self.has_pinned_stories,
                self.is_blocked_for_stories,
                self.wallpaper_overridden,
                self.read_dates_private,
                self.contact_require_premium,
                has_business_info,
                has_birthdate,
                has_personal_channel_id,
                self.sponsored_enabled,
                has_flags2,
            ],
        );
        if has_flags2 {
            store_flags(
                out,
                [
                    self.has_preview_medias,
                    has_privacy_policy_url,
                    has_gift_count,
                    self.can_view_revenue,
                    self.can_manage_emoji_status,
                    has_placeholder_path,
                    has_background_color,
                    has_background_dark_color,
                    has_header_color,
                    has_header_dark_color,
                    has_referral_program_info,
                    has_verifier_settings,
                    has_bot_verification,
                    has_charge_paid_message_stars,
                    has_send_paid_message_stars,
                    has_gift_settings,
                    has_star_rating,
                    has_pending_star_rating,
                    has_main_profile_tab,
                    has_first_saved_music_file_id,
                    has_note,
                    self.noforwards_my_enabled,
                    self.noforwards_peer_enabled,
                    has_manager_bot_user_id,
                    self.unofficial_security_risk,
                    has_linked_community_id,
                ],
            );
        }
        if has_about {
            store_string(out, &self.about);
        }
        store_i32(out, self.common_chat_count);
        TimeCtx::store_time(out, self.expires_at, ctx);
        if has_private_forward_name {
            store_string(out, &self.private_forward_name);
        }
        if has_birthdate {
            self.birthdate.store(out);
        }
        if has_personal_channel_id {
            store_i64(out, self.personal_channel_id);
        }
        if has_gift_count {
            store_i32(out, self.gift_count);
        }
        if has_charge_paid_message_stars {
            store_i64(out, self.charge_paid_message_stars);
        }
        if has_send_paid_message_stars {
            store_i64(out, self.send_paid_message_stars);
        }
        if has_linked_community_id {
            store_i64(out, self.linked_community_id.get());
        }
    }

    /// `log_event_store(UserFull)` — the version-prefixed database value.
    pub fn store_value(&self, out: &mut Vec<u8>, ctx: &TimeCtx) {
        store_parse::store_log_event_version(out);
        self.store(out, ctx);
    }

    /// `log_event_parse(UserFull)` — the supported subset; unsupported
    /// C++ flags produce a clean parse error (module docs).
    pub fn parse_value(input: &[u8], ctx: &TimeCtx) -> Result<UserFull, &'static str> {
        let mut parser = Parser::new_log_event(input)?;
        let result = UserFull::parse_impl(&mut parser, ctx)?;
        parser.fetch_end()?;
        Ok(result)
    }

    fn parse_impl(parser: &mut Parser, ctx: &TimeCtx) -> Result<UserFull, &'static str> {
        let [
            has_about,
            is_blocked,
            can_be_called,
            has_private_calls,
            can_pin_messages,
            need_phone_number_privacy_exception,
            has_photo,
            supports_video_calls,
            has_description,
            has_commands,
            has_private_forward_name,
            has_group_administrator_rights,
            has_broadcast_administrator_rights,
            has_menu_button,
            has_description_photo,
            has_description_animation,
            legacy_has_premium_gift_options,
            voice_messages_forbidden,
            has_personal_photo,
            has_fallback_photo,
            has_pinned_stories,
            is_blocked_for_stories,
            wallpaper_overridden,
            read_dates_private,
            contact_require_premium,
            has_business_info,
            has_birthdate,
            has_personal_channel_id,
            sponsored_enabled,
            has_flags2,
        ] = parser.parse_flags()?;
        if has_photo || has_personal_photo || has_fallback_photo {
            return Err("UserFull photos need the FileManager format (Files step)");
        }
        if has_description
            || has_commands
            || has_group_administrator_rights
            || has_broadcast_administrator_rights
            || has_menu_button
            || has_description_photo
            || has_description_animation
            || has_business_info
        {
            return Err("UserFull bot/business fields need their subsystems");
        }
        if legacy_has_premium_gift_options {
            return Err("legacy premium gift options are unsupported");
        }
        let [
            has_preview_medias,
            has_privacy_policy_url,
            has_gift_count,
            can_view_revenue,
            can_manage_emoji_status,
            has_placeholder_path,
            has_background_color,
            has_background_dark_color,
            has_header_color,
            has_header_dark_color,
            has_referral_program_info,
            has_verifier_settings,
            has_bot_verification,
            has_charge_paid_message_stars,
            has_send_paid_message_stars,
            has_gift_settings,
            has_star_rating,
            has_pending_star_rating,
            has_main_profile_tab,
            has_first_saved_music_file_id,
            has_note,
            noforwards_my_enabled,
            noforwards_peer_enabled,
            has_manager_bot_user_id,
            unofficial_security_risk,
            has_linked_community_id,
        ] = if has_flags2 {
            parser.parse_flags()?
        } else {
            [false; 26]
        };
        if has_privacy_policy_url
            || has_placeholder_path
            || has_background_color
            || has_background_dark_color
            || has_header_color
            || has_header_dark_color
            || has_referral_program_info
            || has_verifier_settings
            || has_bot_verification
            || has_gift_settings
            || has_star_rating
            || has_pending_star_rating
            || has_main_profile_tab
            || has_first_saved_music_file_id
            || has_note
            || has_manager_bot_user_id
        {
            return Err("UserFull bot/gift/note fields need their subsystems");
        }

        let mut result = UserFull::new();
        result.is_blocked = is_blocked;
        result.can_be_called = can_be_called;
        result.has_private_calls = has_private_calls;
        result.can_pin_messages = can_pin_messages;
        result.need_phone_number_privacy_exception = need_phone_number_privacy_exception;
        result.supports_video_calls = supports_video_calls;
        result.voice_messages_forbidden = voice_messages_forbidden;
        result.has_pinned_stories = has_pinned_stories;
        result.is_blocked_for_stories = is_blocked_for_stories;
        result.wallpaper_overridden = wallpaper_overridden;
        result.read_dates_private = read_dates_private;
        result.contact_require_premium = contact_require_premium;
        result.sponsored_enabled = sponsored_enabled;
        result.has_preview_medias = has_preview_medias;
        result.can_view_revenue = can_view_revenue;
        result.can_manage_emoji_status = can_manage_emoji_status;
        result.noforwards_my_enabled = noforwards_my_enabled;
        result.noforwards_peer_enabled = noforwards_peer_enabled;
        result.unofficial_security_risk = unofficial_security_risk;

        if has_about {
            result.about = parser.string()?;
        }
        result.common_chat_count = parser.i32_()?;
        result.expires_at = TimeCtx::parse_time(parser, ctx)?;
        if has_private_forward_name {
            result.private_forward_name = parser.string()?;
        }
        if has_birthdate {
            result.birthdate = Birthdate::parse_from(parser)?;
        }
        if has_personal_channel_id {
            result.personal_channel_id = parser.i64_()?;
        }
        if has_gift_count {
            result.gift_count = parser.i32_()?;
        }
        if has_charge_paid_message_stars {
            result.charge_paid_message_stars = parser.i64_()?;
        }
        if has_send_paid_message_stars {
            result.send_paid_message_stars = parser.i64_()?;
        }
        if has_linked_community_id {
            result.linked_community_id = CommunityId::new(parser.i64_()?);
        }
        Ok(result)
    }
}

/// `td::UserManager::SecretChat` (UserManager.h:811-839).
#[derive(Debug)]
pub struct SecretChat {
    pub access_hash: i64,
    pub user_id: UserId,
    pub state: SecretChatState,
    pub key_hash: String,
    pub ttl: i32,
    pub date: i32,
    pub layer: i32,
    pub initial_folder_id: i32,

    pub is_outbound: bool,

    pub is_ttl_changed: bool,
    pub is_state_changed: bool,
    pub is_being_updated: bool,
    pub is_changed: bool,
    pub need_save_to_database: bool,

    pub is_saved: bool,
    pub is_being_saved: bool,
}

impl Default for SecretChat {
    fn default() -> Self {
        SecretChat {
            access_hash: 0,
            user_id: UserId::default(),
            state: SecretChatState::Unknown,
            key_hash: String::new(),
            ttl: 0,
            date: 0,
            layer: 0,
            initial_folder_id: 0,
            is_outbound: false,
            is_ttl_changed: true,
            is_state_changed: true,
            is_being_updated: false,
            is_changed: true,
            need_save_to_database: true,
            is_saved: false,
            is_being_saved: false,
        }
    }
}

impl SecretChat {
    pub fn new() -> SecretChat {
        SecretChat::default()
    }

    /// `SecretChat::store` (UserManager.cpp:2613-2635).
    pub fn store(&self, out: &mut Vec<u8>) {
        use store_parse::*;
        let has_layer = self.layer > SECRET_CHAT_DEFAULT_LAYER;
        let has_initial_folder_id = self.initial_folder_id != 0;
        store_flags(out, [self.is_outbound, has_layer, has_initial_folder_id]);
        store_i64(out, self.access_hash);
        store_i64(out, self.user_id.get());
        store_i32(out, self.state as i32);
        store_i32(out, self.ttl);
        store_i32(out, self.date);
        store_string(out, &self.key_hash);
        if has_layer {
            store_i32(out, self.layer);
        }
        if has_initial_folder_id {
            store_i32(out, self.initial_folder_id);
        }
    }

    /// `log_event_store(SecretChat)` — the version-prefixed database value.
    pub fn store_value(&self, out: &mut Vec<u8>) {
        store_parse::store_log_event_version(out);
        self.store(out);
    }

    /// `log_event_parse(SecretChat)` (UserManager.cpp:2638-2666).
    pub fn parse_value(input: &[u8]) -> Result<SecretChat, &'static str> {
        use store_parse::version;
        let mut parser = Parser::new_log_event(input)?;
        let [is_outbound, has_layer, has_initial_folder_id] = parser.parse_flags()?;
        let mut result = SecretChat::new();
        result.is_outbound = is_outbound;
        if parser.version() >= version::ADD_ACCESS_HASH_TO_SECRET_CHAT {
            result.access_hash = parser.i64_()?;
        }
        result.user_id = UserId::new(parser.i64_()?);
        result.state = match parser.i32_()? {
            -1 => SecretChatState::Unknown,
            0 => SecretChatState::Waiting,
            1 => SecretChatState::Active,
            2 => SecretChatState::Closed,
            _ => return Err("Unknown SecretChatState"),
        };
        result.ttl = parser.i32_()?;
        result.date = parser.i32_()?;
        if parser.version() >= version::ADD_KEY_HASH_TO_SECRET_CHAT {
            result.key_hash = parser.string()?;
        }
        if has_layer {
            result.layer = parser.i32_()?;
        } else {
            result.layer = SECRET_CHAT_DEFAULT_LAYER;
        }
        if has_initial_folder_id {
            result.initial_folder_id = parser.i32_()?;
        }
        parser.fetch_end()?;
        Ok(result)
    }
}

/// The shared-context slice every manager method needs — the `Td *td_` of
/// the C++, narrowed to the pieces the user manager touches.
pub struct UserEnv<'a> {
    pub global: &'a mut Global,
    pub options: &'a mut OptionManager,
    /// `td_->auth_manager_->is_bot()`.
    pub auth_is_bot: bool,
}

impl<'a> UserEnv<'a> {
    pub fn unix_time(&self) -> i32 {
        self.global.unix_time()
    }

    pub fn server_time(&self) -> f64 {
        self.global.server_time()
    }

    pub fn time_ctx(&self) -> TimeCtx {
        TimeCtx::from_global(self.global)
    }

    pub fn restriction_options(&self) -> RestrictionOptions {
        RestrictionOptions {
            ignored_restriction_reasons: self
                .options
                .get_option_string("ignored_restriction_reasons", ""),
            restriction_add_platforms: self
                .options
                .get_option_string("restriction_add_platforms", ""),
            ignore_platform_restrictions: self
                .options
                .get_option_boolean("ignore_platform_restrictions", false),
        }
    }
}

/// `td::AccessRights` (AccessRights.h) — the subset the user manager uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessRights {
    Know,
    Read,
    Write,
}

/// The User change flags `update_user` consumes, one at a time (the C++
/// reads and clears `u->is_*` directly).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UserFlag {
    IsContact,
    IsMutualContact,
    IsDeleted,
    IsPremium,
    Name,
    Username,
    Photo,
    AccentColor,
    PhoneNumber,
    HasBotForumView,
    StoriesHidden,
    EmojiStatusChanged,
    FullInfo,
    OnlineStatus,
}

/// `td::UserManager` — the in-process manager (the C++ actor collapses
/// into the Td actor, like the M.1 AuthManager).
#[derive(Default)]
pub struct UserManager {
    pub(crate) my_id: UserId,
    users: BTreeMap<i64, User>,
    users_full: BTreeMap<i64, UserFull>,
    secret_chats: BTreeMap<i32, SecretChat>,
    unknown_users: BTreeSet<i64>,
    unknown_secret_chats: BTreeSet<i32>,
    loaded_from_database_users: BTreeSet<i64>,

    resolved_phone_numbers: BTreeMap<String, UserId>,
    restricted_user_ids: BTreeSet<i64>,

    my_was_online_local: i32,
    was_online_local: i32,
    was_online_remote: i32,
    next_set_my_active_users: f64,

    /// The outgoing td_api updates (`Td::send_update` fan-out), drained by
    /// the Td actor after each manager call.
    pending_updates: Vec<td_api::Update>,

    /// `user_online_timeout_` deadlines (the MultiTimeout replacement).
    user_online_timeouts: BTreeMap<i64, f64>,
    /// `user_emoji_status_timeout_` deadlines.
    user_emoji_status_timeouts: BTreeMap<i64, f64>,
}

impl UserManager {
    pub fn new() -> UserManager {
        UserManager::default()
    }

    pub fn take_updates(&mut self) -> Vec<td_api::Update> {
        std::mem::take(&mut self.pending_updates)
    }

    fn send_update(&mut self, update: td_api::Update) {
        self.pending_updates.push(update);
    }

    /// `UserManager::load_my_id` (UserManager.cpp:2975-2992) — the static
    /// binlog-pmc reader, also used by the AuthManager restore path.
    pub fn load_my_id(binlog_pmc: &mut BinlogKeyValue) -> UserId {
        let id_string = binlog_pmc.get("my_id");
        if !id_string.is_empty() {
            let my_id = UserId::new(id_string.parse::<i64>().unwrap_or(0));
            if my_id.is_valid() {
                return my_id;
            }

            // the legacy "user<id>" form
            if let Some(tail) = id_string.strip_prefix("user") {
                let my_id = UserId::new(tail.parse::<i64>().unwrap_or(0));
                if my_id.is_valid() {
                    binlog_pmc.set("my_id", &my_id.get().to_string());
                    return my_id;
                }
            }
            tdutils::log_error!("Wrong my ID = \"{id_string}\" stored in database");
        }
        UserId::default()
    }

    /// The `UserManager` constructor's `my_id_ = load_my_id()` line
    /// (UserManager.cpp:2824) — the Td init calls it once the binlog pmc
    /// exists (the option-side publish happens in the AuthManager restore).
    pub fn init_my_id(&mut self, binlog_pmc: &mut BinlogKeyValue) {
        self.my_id = Self::load_my_id(binlog_pmc);
    }

    /// The constructor's `my_was_online_local` / `my_was_online_remote`
    /// binlog-pmc restore (UserManager.cpp:2765-2771); the freeze-state
    /// half of the block arrives with M.4.15. `is_online` is
    /// `OnlineManager::is_online()` — `false` until its step lands.
    pub fn init_my_online_status(
        &mut self,
        binlog_pmc: &mut BinlogKeyValue,
        unix_time: i32,
        is_online: bool,
    ) {
        self.was_online_local = binlog_pmc.get("my_was_online_local").parse().unwrap_or(0);
        self.was_online_remote = binlog_pmc.get("my_was_online_remote").parse().unwrap_or(0);
        if self.was_online_local >= unix_time && !is_online {
            self.was_online_local = unix_time - 1;
        }
    }

    /// `UserManager::get_my_id`.
    pub fn get_my_id(&self) -> UserId {
        if !self.my_id.is_valid() {
            tdutils::log_error!("Wrong or unknown my ID returned");
        }
        self.my_id
    }

    /// `UserManager::set_my_id` (UserManager.cpp:2999-3017).
    pub fn set_my_id(&mut self, env: &mut UserEnv, my_id: UserId) {
        let my_old_id = self.my_id;
        if my_old_id.is_valid() && my_old_id != my_id {
            tdutils::log_error!(
                "Already know that me is {my_old_id:?} but received userSelf with {my_id:?}"
            );
            return;
        }
        if !my_id.is_valid() {
            tdutils::log_error!("Receive invalid my ID {my_id:?}");
            return;
        }
        if my_old_id != my_id {
            self.my_id = my_id;
            env.global
                .binlog_pmc()
                .set("my_id", &my_id.get().to_string());
            env.options.set_option_integer("my_id", my_id.get());
            // `force_sync` — the synchronous kv needs no separate flush
        }
    }

    /// `UserManager::get_service_notifications_user_id`.
    pub fn get_service_notifications_user_id() -> UserId {
        UserId::new(777000)
    }

    /// `UserManager::get_replies_bot_user_id`.
    pub fn get_replies_bot_user_id(is_test_dc: bool) -> UserId {
        UserId::new(if is_test_dc { 708513 } else { 1271266957 })
    }

    /// `UserManager::get_verification_codes_bot_user_id`.
    pub fn get_verification_codes_bot_user_id() -> UserId {
        UserId::new(489000)
    }

    /// `UserManager::get_anonymous_bot_user_id`.
    pub fn get_anonymous_bot_user_id(is_test_dc: bool) -> UserId {
        UserId::new(if is_test_dc { 552888 } else { 1087968824 })
    }

    /// `UserManager::get_channel_bot_user_id`.
    pub fn get_channel_bot_user_id(is_test_dc: bool) -> UserId {
        UserId::new(if is_test_dc { 936174 } else { 136817688 })
    }

    /// `UserManager::get_anti_spam_bot_user_id`.
    pub fn get_anti_spam_bot_user_id(is_test_dc: bool) -> UserId {
        UserId::new(if is_test_dc { 2200583762 } else { 5434988373 })
    }

    /// The special-user table of `get_user_force` (UserManager.cpp:5022-5025).
    fn is_special_user_id(user_id: UserId, is_test_dc: bool) -> bool {
        user_id == Self::get_service_notifications_user_id()
            || user_id == Self::get_replies_bot_user_id(is_test_dc)
            || user_id == Self::get_verification_codes_bot_user_id()
            || user_id == Self::get_anonymous_bot_user_id(is_test_dc)
            || user_id == Self::get_channel_bot_user_id(is_test_dc)
            || user_id == Self::get_anti_spam_bot_user_id(is_test_dc)
    }

    pub fn get_user(&self, user_id: UserId) -> Option<&User> {
        self.users.get(&user_id.get())
    }

    pub fn get_user_full(&self, user_id: UserId) -> Option<&UserFull> {
        self.users_full.get(&user_id.get())
    }

    /// `UserManager::add_user` (UserManager.cpp:4841-4848).
    fn add_user(&mut self, user_id: UserId) -> &mut User {
        assert!(user_id.is_valid());
        self.users.entry(user_id.get()).or_default()
    }

    /// `UserManager::have_min_user` (UserManager.cpp:4829-4832).
    pub fn have_min_user(&self, user_id: UserId) -> bool {
        self.get_user(user_id).is_some()
    }

    /// `UserManager::have_accessible_user` (UserManager.cpp:4824-4828);
    /// the message-registered branch lands with MessagesManager.
    pub fn have_accessible_user(&self, user_id: UserId) -> bool {
        self.have_min_user(user_id)
    }

    /// `UserManager::get_user_database_key` (UserManager.cpp:4870-4872).
    fn get_user_database_key(user_id: UserId) -> Vec<u8> {
        format!("us{}", user_id.get()).into_bytes()
    }

    /// `UserManager::save_user` (UserManager.cpp:4850-4868): while the user
    /// is unsaved, mirror it into the binlog (`binlog_add` the first time,
    /// `binlog_rewrite` after) so a crash before the sqlite flush replays,
    /// then push the row into the "common" kv.
    fn save_user(&mut self, env: &mut UserEnv, user_id: UserId, from_binlog: bool) {
        if !env.global.use_chat_info_database() {
            return;
        }
        let (need_save, log_event_id) = match self.users.get(&user_id.get()) {
            Some(u) => (!u.is_saved || !u.is_status_saved, u.log_event_id),
            None => return,
        };
        if need_save {
            if !from_binlog {
                let data = {
                    let u = self.users.get(&user_id.get()).unwrap();
                    let ctx = env.time_ctx();
                    let mut data = Vec::new();
                    store_user_log_event(&mut data, user_id, u, &ctx);
                    data
                };
                let binlog = env.global.td_db().binlog();
                let new_id = if log_event_id == 0 {
                    binlog.add(tddb::td_db::LOG_EVENT_HANDLER_TYPE_USERS, &data)
                } else {
                    binlog.rewrite(
                        log_event_id,
                        tddb::td_db::LOG_EVENT_HANDLER_TYPE_USERS,
                        &data,
                    )
                };
                self.users.get_mut(&user_id.get()).unwrap().log_event_id = new_id;
            }

            self.save_user_to_database(env, user_id);
        }
    }

    /// `UserManager::save_user_to_database` + `_impl` +
    /// `on_save_user_to_database` collapsed onto the synchronous kv: the
    /// C++ guards (load-before-save sequencing, the `is_being_saved`
    /// window) exist to order against the async pmc and cannot interleave
    /// here; the write is infallible, so the failure branch
    /// (`is_saved = false` + re-save) is unreachable.
    fn save_user_to_database(&mut self, env: &mut UserEnv, user_id: UserId) {
        let value = {
            let Some(u) = self.users.get_mut(&user_id.get()) else {
                return;
            };
            if u.is_being_saved {
                return;
            }
            u.is_being_saved = true;
            u.is_saved = true;
            u.is_status_saved = true;
            let ctx = env.time_ctx();
            let mut value = Vec::new();
            u.store_value(&mut value, &ctx);
            value
        };
        env.global
            .td_db()
            .common_kv()
            .set(&Self::get_user_database_key(user_id), &value);
        let u = self.users.get_mut(&user_id.get()).unwrap();
        u.is_being_saved = false;
        // `on_save_user_to_database`: the row is durable, retire the binlog
        // mirror.
        if u.log_event_id != 0 {
            let log_event_id = u.log_event_id;
            env.global.td_db().binlog().erase(log_event_id);
            u.log_event_id = 0;
        }
    }

    /// `UserManager::load_user_from_database_impl` +
    /// `on_load_user_from_database` (UserManager.cpp:4952-5014) collapsed
    /// onto the synchronous kv (the per-id promise queue exists only to
    /// batch the async completions).
    fn load_user_from_database(&mut self, env: &mut UserEnv, user_id: UserId, force: bool) {
        if env.global.close_flag() && !force {
            return;
        }
        if !self.loaded_from_database_users.insert(user_id.get()) {
            return;
        }
        let value = env
            .global
            .td_db()
            .common_kv()
            .get(&Self::get_user_database_key(user_id));
        if let Some(u) = self.users.get_mut(&user_id.get()) {
            // The C++ races an in-memory user against the async load; here
            // the only entry is a binlog-replayed user: keep the newer
            // memory state if it differs from the row, else retire the
            // mirror.
            let ctx = env.time_ctx();
            let mut new_value = Vec::new();
            u.store_value(&mut new_value, &ctx);
            let erase_log_event_id = if value != new_value {
                None
            } else if u.log_event_id != 0 {
                Some(std::mem::take(&mut u.log_event_id))
            } else {
                return;
            };
            if let Some(log_event_id) = erase_log_event_id {
                env.global.td_db().binlog().erase(log_event_id);
                return;
            }
            self.save_user_to_database(env, user_id);
            return;
        }
        if value.is_empty() {
            return;
        }
        let ctx = env.time_ctx();
        match User::parse_value(&value, &ctx) {
            Ok(user) => {
                let u = self.add_user(user_id);
                *u = user;
                let u = self.users.get_mut(&user_id.get()).unwrap();
                u.is_saved = true;
                u.is_status_saved = true;
                self.update_user(env, user_id, true, true);
            }
            Err(error) => {
                tdutils::log_error!("Failed to load {user_id:?} from database: {error}");
                self.users.remove(&user_id.get());
            }
        }
    }

    /// `UserManager::on_binlog_user_event` (UserManager.cpp:3460-3487): the
    /// crash-window replay — restore the user from its binlog mirror and
    /// keep the event id so the save rewrites (not duplicates) it.
    pub fn on_binlog_user_event(
        &mut self,
        env: &mut UserEnv,
        event: &tddb::binlog::event::BinlogEvent,
    ) {
        let binlog = env.global.td_db().binlog();
        if !env.global.use_chat_info_database() {
            binlog.erase(event.id);
            return;
        }

        let ctx = env.time_ctx();
        let parsed = parse_user_log_event(event.get_data(), &ctx);
        let (user_id, user) = match parsed {
            Ok(parsed) => parsed,
            Err(error) => {
                tdutils::log_error!("Failed to load a user from binlog: {error}");
                binlog.erase(event.id);
                return;
            }
        };
        if self.have_min_user(user_id) || !user_id.is_valid() {
            tdutils::log_error!("Skip adding already added {user_id:?}");
            binlog.erase(event.id);
            return;
        }

        let u = self.add_user(user_id);
        *u = user;
        let u = self.users.get_mut(&user_id.get()).unwrap();
        u.log_event_id = event.id;

        self.update_user(env, user_id, true, false);
    }

    /// `UserManager::get_user_force_impl` (UserManager.cpp:5120-5139).
    fn get_user_force_impl(&mut self, env: &mut UserEnv, user_id: UserId, _source: &str) -> bool {
        if !user_id.is_valid() {
            return false;
        }
        if self.users.contains_key(&user_id.get()) {
            return true;
        }
        if !env.global.use_chat_info_database() {
            return false;
        }
        if self.loaded_from_database_users.contains(&user_id.get()) {
            return false;
        }
        self.load_user_from_database(env, user_id, true);
        self.users.contains_key(&user_id.get())
    }

    /// `UserManager::get_user_force` (UserManager.cpp:5016-5118): db
    /// preload, then the special-user synthesis. Returns whether the user
    /// is now in the cache.
    pub fn get_user_force(&mut self, env: &mut UserEnv, user_id: UserId, source: &str) -> bool {
        let mut loaded = self.get_user_force_impl(env, user_id, source);
        let is_received = self
            .users
            .get(&user_id.get())
            .map(|u| u.is_received)
            .unwrap_or(false);
        if (!loaded || !is_received) && Self::is_special_user_id(user_id, env.global.is_test_dc()) {
            let user = self.make_special_user(env, user_id);
            self.on_get_user(env, user, "get_user_force");
            loaded = self.users.contains_key(&user_id.get());
            // `reload_user` refreshes the special users from the server;
            // the query lands with the net step.
        }
        loaded
    }

    /// The synthetic `telegram_api::user` of `get_user_force`
    /// (UserManager.cpp:5040-5110).
    fn make_special_user(&self, env: &UserEnv, user_id: UserId) -> telegram_api::User {
        // returns the umbrella variant

        let is_test_dc = env.global.is_test_dc();
        let mut flags = USER_ACCESS_HASH_MASK;
        let need_apply_min_photo = true;
        let mut is_bot = false;
        let mut is_private_bot = false;
        let mut is_verified = false;
        let mut is_support = false;
        let mut profile_photo_id = 0i64;
        let profile_photo_dc_id = 1;
        let mut first_name = String::new();
        let mut username = String::new();
        let mut phone_number = String::new();
        let mut bot_info_version = 0;

        if user_id == Self::get_service_notifications_user_id() {
            is_verified = true;
            is_support = true;
            first_name = "Telegram".to_owned();
            if !is_test_dc {
                profile_photo_id = 3337190045231036;
            }
            phone_number = "42777".to_owned();
        } else if user_id == Self::get_replies_bot_user_id(is_test_dc) {
            is_bot = true;
            if !is_test_dc {
                is_private_bot = true;
            }
            first_name = "Replies".to_owned();
            username = "replies".to_owned();
            bot_info_version = if is_test_dc { 1 } else { 3 };
        } else if user_id == Self::get_verification_codes_bot_user_id() {
            is_bot = true;
            is_private_bot = true;
            is_verified = true;
            first_name = "Verification Codes".to_owned();
            username = "VerificationCodes".to_owned();
            bot_info_version = if is_test_dc { 4 } else { 2 };
        } else if user_id == Self::get_anonymous_bot_user_id(is_test_dc) {
            is_bot = true;
            if !is_test_dc {
                is_private_bot = true;
            }
            first_name = "Group".to_owned();
            username = if is_test_dc {
                "izgroupbot"
            } else {
                "GroupAnonymousBot"
            }
            .to_owned();
            bot_info_version = if is_test_dc { 1 } else { 3 };
            profile_photo_id = 5159307831025969322;
        } else if user_id == Self::get_channel_bot_user_id(is_test_dc) {
            is_bot = true;
            if !is_test_dc {
                is_private_bot = true;
            }
            first_name = if is_test_dc { "Channels" } else { "Channel" }.to_owned();
            username = if is_test_dc {
                "channelsbot"
            } else {
                "Channel_Bot"
            }
            .to_owned();
            bot_info_version = if is_test_dc { 1 } else { 4 };
            profile_photo_id = 587627495930570665;
        } else if user_id == Self::get_anti_spam_bot_user_id(is_test_dc) {
            is_bot = true;
            if is_test_dc {
                first_name = "antispambot".to_owned();
                username = "tantispambot".to_owned();
            } else {
                is_verified = true;
                first_name = "Telegram Anti-Spam".to_owned();
                username = "tgsantispambot".to_owned();
                profile_photo_id = 5170408289966598902;
            }
        }
        if !phone_number.is_empty() {
            flags |= USER_PHONE_MASK;
        }

        let mut user = telegram_api::user {
            flags,
            bot: is_bot,
            bot_nochats: is_private_bot,
            verified: is_verified,
            support: is_support,
            apply_min_photo: need_apply_min_photo,
            id: user_id.get(),
            access_hash: 1,
            first_name,
            username,
            phone: phone_number,
            bot_info_version,
            ..telegram_api::user::default()
        };
        // the test DCs carry no profile photo for the special users
        if !is_test_dc && profile_photo_id != 0 {
            user.flags |= 1 << 5; // photo:flags.5
            user.photo =
                telegram_api::UserProfilePhoto::userProfilePhoto(telegram_api::userProfilePhoto {
                    flags: 0,
                    has_video: false,
                    personal: false,
                    photo_id: profile_photo_id,
                    stripped_thumb: Vec::new(),
                    dc_id: profile_photo_dc_id,
                });
        }
        telegram_api::User::user(user)
    }

    /// `UserManager::on_get_user` (UserManager.cpp:3127-3452). Returns the
    /// user id (`UserId::default()` for the invalid constructors).
    pub fn on_get_user(
        &mut self,
        env: &mut UserEnv,
        user_ptr: telegram_api::User,
        source: &str,
    ) -> UserId {
        match user_ptr {
            telegram_api::User::userEmpty(user) => {
                let user_id = UserId::new(user.id);
                if !user_id.is_valid() {
                    tdutils::log_error!("Receive invalid {user_id:?} from {source}");
                    return UserId::default();
                }
                let known = self.get_user_force_impl(env, user_id, source);
                if !known && source != "GetUsersQuery" {
                    // userEmpty should be received only through getUsers
                    tdutils::log_error!(
                        "Have no information about {user_id:?}, but received userEmpty from {source}"
                    );
                }
                user_id
            }
            telegram_api::User::user(user) => self.on_get_real_user(env, user, source),
        }
    }

    /// The `telegram_api::user` branch of `on_get_user`.
    #[allow(clippy::too_many_lines)]
    fn on_get_real_user(
        &mut self,
        env: &mut UserEnv,
        mut user: telegram_api::user,
        source: &str,
    ) -> UserId {
        let user_id = UserId::new(user.id);
        if !user_id.is_valid() {
            tdutils::log_error!("Receive invalid {user_id:?}");
            return UserId::default();
        }

        let mut is_bot = user.bot;
        if user.self_ {
            let phone = user.phone.clone();
            self.set_my_id(env, user_id);
            if !is_bot {
                env.options.set_option_string("my_phone_number", &phone);
            }
        }

        let flags = user.flags;
        let have_access_hash = (flags & USER_ACCESS_HASH_MASK) != 0;
        let is_received = !user.min;
        let is_contact = user.contact;
        let is_deleted = user.deleted;

        let user_exists = self.users.contains_key(&user_id.get());
        if !user_exists {
            if !is_received {
                // we must preload received inaccessible users from database
                // in order to not save the min-user to the database and to
                // not override access_hash and other data
                let preloaded = self.get_user_force_impl(env, user_id, "on_get_user 2");
                if !preloaded {
                    self.add_user(user_id);
                }
            } else if is_contact && !env.auth_is_bot {
                // preload contact users from database to know that
                // is_contact didn't change (are_contacts_loaded_ is false
                // until the Contacts step)
                let preloaded = self.get_user_force_impl(env, user_id, "on_get_user 3");
                if !preloaded {
                    self.add_user(user_id);
                }
            } else {
                self.add_user(user_id);
            }
            if self.unknown_users.remove(&user_id.get()) {
                let u = self.users.get_mut(&user_id.get()).unwrap();
                u.is_photo_inited = true;
            }
        } else if is_deleted && !is_received && user_id == self.get_my_id() {
            // Ignore self as frozen min-user
            return user_id;
        }

        if have_access_hash {
            // access_hash must be updated before photo
            let access_hash = user.access_hash;
            let is_min_access_hash =
                !(is_received || (flags & USER_PHONE_MASK) != 0 && user.phone.is_empty());
            let u = self.users.get_mut(&user_id.get()).unwrap();
            if u.access_hash != access_hash
                && (!is_min_access_hash || u.is_min_access_hash || u.access_hash == -1)
            {
                u.access_hash = access_hash;
                u.is_min_access_hash = is_min_access_hash;
                u.need_save_to_database = true;
            }
        }

        let mut is_verified = user.verified;
        let mut is_premium = user.premium;
        let mut is_support = user.support;
        let mut can_join_groups = !user.bot_nochats;
        let mut can_read_all_group_messages = user.bot_chat_history;
        let mut can_be_added_to_attach_menu = user.bot_attach_menu;
        let mut has_main_app = user.bot_has_main_app;
        let mut has_bot_forum_view = user.bot_forum_view;
        let mut can_bot_create_topics = has_bot_forum_view && !user.bot_forum_can_manage_topics;
        let mut can_manage_bots = user.bot_can_manage_bots;
        let attach_menu_enabled = user.attach_menu_enabled;
        let is_scam = user.scam;
        let mut can_be_edited_bot = user.bot_can_edit;
        let mut is_inline_bot = (flags & USER_BOT_INLINE_PLACEHOLDER_MASK) != 0;
        let mut is_guestchat_bot = user.bot_guestchat;
        let mut is_business_bot = user.bot_business;
        let mut is_guard_bot = user.bot_guard;
        let mut inline_query_placeholder = std::mem::take(&mut user.bot_inline_placeholder);
        let mut bot_active_users = user.bot_active_users;
        let mut need_location_bot = user.bot_inline_geo;
        let mut need_apply_min_photo = user.apply_min_photo;
        let is_fake = user.fake;
        let stories_available =
            (user.flags2 & USER_STORIES_MAX_ID_MASK) != 0 && user.stories_max_id.max_id > 0;
        let stories_unavailable = user.stories_unavailable;
        let stories_hidden = user.stories_hidden;
        let contact_require_premium = user.contact_require_premium;
        let mut paid_message_star_count = get_star_count(user.send_paid_messages_stars);

        if !is_bot
            && (!can_join_groups
                || can_read_all_group_messages
                || can_be_added_to_attach_menu
                || can_be_edited_bot
                || has_main_app
                || has_bot_forum_view
                || can_bot_create_topics
                || can_manage_bots
                || is_inline_bot
                || is_business_bot
                || is_guestchat_bot
                || is_guard_bot)
        {
            tdutils::log_error!("Receive not bot {user_id:?} with bot properties from {source}");
            can_join_groups = true;
            can_read_all_group_messages = false;
            can_be_added_to_attach_menu = false;
            can_be_edited_bot = false;
            has_main_app = false;
            has_bot_forum_view = false;
            can_bot_create_topics = false;
            can_manage_bots = false;
            is_inline_bot = false;
            is_business_bot = false;
            is_guestchat_bot = false;
            is_guard_bot = false;
        }
        if need_location_bot && !is_inline_bot {
            tdutils::log_error!(
                "Receive not inline bot {user_id:?} which needs user location from {source}"
            );
            need_location_bot = false;
        }

        if is_deleted {
            // just in case
            is_verified = false;
            is_premium = false;
            is_support = false;
            is_bot = false;
            can_join_groups = false;
            can_read_all_group_messages = false;
            can_be_added_to_attach_menu = false;
            can_be_edited_bot = false;
            has_main_app = false;
            has_bot_forum_view = false;
            can_bot_create_topics = false;
            can_manage_bots = false;
            is_inline_bot = false;
            is_guestchat_bot = false;
            is_business_bot = false;
            is_guard_bot = false;
            inline_query_placeholder = String::new();
            bot_active_users = 0;
            need_location_bot = false;
            need_apply_min_photo = false;
            paid_message_star_count = 0;
        }

        let bot_info_version = if is_bot { user.bot_info_version } else { -1 };
        {
            let u = self.users.get_mut(&user_id.get()).unwrap();
            if is_verified != u.is_verified
                || is_support != u.is_support
                || is_bot != u.is_bot
                || can_join_groups != u.can_join_groups
                || can_read_all_group_messages != u.can_read_all_group_messages
                || is_scam != u.is_scam
                || is_fake != u.is_fake
                || is_inline_bot != u.is_inline_bot
                || is_business_bot != u.is_business_bot
                || inline_query_placeholder != u.inline_query_placeholder
                || need_location_bot != u.need_location_bot
                || can_be_added_to_attach_menu != u.can_be_added_to_attach_menu
                || has_main_app != u.has_main_app
                || can_bot_create_topics != u.can_bot_create_topics
                || can_manage_bots != u.can_manage_bots
                || is_guestchat_bot != u.is_guestchat_bot
                || is_guard_bot != u.is_guard_bot
            {
                if is_bot != u.is_bot {
                    u.is_full_info_changed = true;
                }
                u.is_verified = is_verified;
                u.is_support = is_support;
                u.is_bot = is_bot;
                u.can_join_groups = can_join_groups;
                u.can_read_all_group_messages = can_read_all_group_messages;
                u.is_scam = is_scam;
                u.is_fake = is_fake;
                u.is_inline_bot = is_inline_bot;
                u.is_business_bot = is_business_bot;
                u.inline_query_placeholder = inline_query_placeholder;
                u.need_location_bot = need_location_bot;
                u.can_be_added_to_attach_menu = can_be_added_to_attach_menu;
                u.has_main_app = has_main_app;
                u.can_bot_create_topics = can_bot_create_topics;
                u.can_manage_bots = can_manage_bots;
                u.is_guestchat_bot = is_guestchat_bot;
                u.is_guard_bot = is_guard_bot;
                u.is_changed = true;
            }
            let is_me = user_id == self.my_id;
            if bot_active_users != u.bot_active_users
                && (!is_me || tdutils::time::now() > self.next_set_my_active_users)
            {
                u.bot_active_users = bot_active_users;
                u.is_changed = true;
                if is_me {
                    self.next_set_my_active_users = tdutils::time::now() + 600.0;
                }
            }
            if contact_require_premium != u.contact_require_premium
                || paid_message_star_count != u.paid_message_star_count
            {
                u.contact_require_premium = contact_require_premium;
                u.paid_message_star_count = paid_message_star_count;
                u.is_changed = true;
                // user_full_contact_price_ — the premium-contact pricing
                // cache lands with the contact queries
            }
            if is_received && attach_menu_enabled != u.attach_menu_enabled {
                u.attach_menu_enabled = attach_menu_enabled;
                u.is_changed = true;
            }
            if is_premium != u.is_premium {
                u.is_premium = is_premium;
                u.is_is_premium_changed = true;
                u.is_changed = true;
                u.is_full_info_changed = true;
            }
            if has_bot_forum_view != u.has_bot_forum_view {
                u.has_bot_forum_view = has_bot_forum_view;
                u.is_has_bot_forum_view_changed = true;
                u.is_changed = true;
            }
            if is_received && can_be_edited_bot != u.can_be_edited_bot {
                u.can_be_edited_bot = can_be_edited_bot;
                u.is_changed = true;
                u.is_full_info_changed = true;
            }
            if u.bot_info_version != bot_info_version {
                u.bot_info_version = bot_info_version;
                u.need_save_to_database = true;
            }
            if is_received && u.need_apply_min_photo != need_apply_min_photo {
                u.need_apply_min_photo = need_apply_min_photo;
                u.need_save_to_database = true;
            }
            if is_received && !u.is_received {
                u.is_received = true;
                u.is_changed = true;
            }
            if is_deleted != u.is_deleted {
                u.is_deleted = is_deleted;
                u.is_is_deleted_changed = true;
                u.is_changed = true;
            }
            if u.language_code != user.lang_code && !user.lang_code.is_empty() {
                u.language_code = user.lang_code.clone();
                u.is_changed = true;
            }
        }

        let is_me_regular_user = !env.auth_is_bot;
        if is_received
            || !self.u_is_received(user_id)
            || self.u_need_apply_min_photo(user_id)
            || self.u_is_deleted(user_id)
        {
            let photo = std::mem::take(&mut user.photo);
            self.on_update_user_photo(env, user_id, photo, source);
        }
        if is_me_regular_user {
            if is_received || !self.u_is_received(user_id) {
                let phone = std::mem::take(&mut user.phone);
                self.on_update_user_phone_number_impl(env, user_id, phone);
            }
            if is_received || !self.u_is_received(user_id) || self.u_was_online(user_id) == 0 {
                let status = std::mem::take(&mut user.status);
                self.on_update_user_online_impl(env, user_id, status);
            }
            if is_received {
                self.on_update_user_is_contact_impl(
                    env,
                    user_id,
                    is_contact,
                    user.mutual_contact,
                    user.close_friend,
                );
            }
        }

        if is_received || !self.u_is_received(user_id) {
            let first_name = std::mem::take(&mut user.first_name);
            let last_name = std::mem::take(&mut user.last_name);
            self.on_update_user_name_impl(user_id, first_name, last_name);
            let usernames = Usernames::from_telegram(
                std::mem::take(&mut user.username),
                std::mem::take(&mut user.usernames),
            );
            self.on_update_user_usernames_impl(env, user_id, usernames);
        }
        let emoji_status = EmojiStatus::get_emoji_status(std::mem::take(&mut user.emoji_status));
        self.on_update_user_emoji_status_impl(user_id, emoji_status);

        if matches!(user.color, telegram_api::PeerColor::peerColorCollectible(_)) {
            let telegram_api::PeerColor::peerColorCollectible(collectible) = std::mem::replace(
                &mut user.color,
                telegram_api::PeerColor::peerColor(Default::default()),
            ) else {
                unreachable!("just matched");
            };
            let collectible = PeerColorCollectible::get_peer_color_collectible(collectible);
            self.on_update_user_colors_impl(
                user_id,
                AccentColorId::default(),
                CustomEmojiId::default(),
                collectible,
            );
        } else {
            let peer_color = PeerColor::from_telegram(&user.color);
            self.on_update_user_colors_impl(
                user_id,
                peer_color.accent_color_id(),
                peer_color.background_custom_emoji_id(),
                None,
            );
        }
        let profile_peer_color = PeerColor::from_telegram(&user.profile_color);
        self.on_update_user_profile_colors_impl(
            user_id,
            profile_peer_color.accent_color_id(),
            profile_peer_color.background_custom_emoji_id(),
        );
        self.on_update_user_linked_community_id_impl(
            user_id,
            CommunityId::new(user.linked_community_id),
        );
        if is_me_regular_user {
            if is_received {
                self.on_update_user_stories_hidden_impl(env, user_id, stories_hidden);
            }
            if stories_available || stories_unavailable {
                // update at the end, because it calls need_poll_user_active_stories
                let live = user.stories_max_id.live;
                let max_id = user.stories_max_id.max_id;
                self.on_update_user_story_ids(user_id, live, max_id, StoryId::default());
            }
            let restriction_reasons = crate::restriction_reason::get_restriction_reasons(
                std::mem::take(&mut user.restriction_reason),
            );
            let u = self.users.get_mut(&user_id.get()).unwrap();
            if restriction_reasons != u.restriction_reasons {
                u.restriction_reasons = restriction_reasons;
                u.is_changed = true;
            }
            self.on_update_user_bot_verification_icon_impl(
                user_id,
                CustomEmojiId::new(user.bot_verification_icon),
            );
        }

        {
            let u = self.users.get_mut(&user_id.get()).unwrap();
            if u.cache_version != USER_CACHE_VERSION && u.is_received {
                u.cache_version = USER_CACHE_VERSION;
                u.need_save_to_database = true;
            }
            u.is_received_from_server = true;
        }
        self.update_user(env, user_id, false, false);
        user_id
    }

    /// `UserManager::on_get_users` (UserManager.cpp:3454-3458).
    pub fn on_get_users(
        &mut self,
        env: &mut UserEnv,
        users: Vec<telegram_api::User>,
        source: &str,
    ) {
        for user in users {
            self.on_get_user(env, user, source);
        }
    }

    /// `UserManager::on_update_user_name` (UserManager.cpp:3520-3546).
    pub fn on_update_user_name(
        &mut self,
        env: &mut UserEnv,
        user_id: UserId,
        first_name: String,
        last_name: String,
    ) {
        if !user_id.is_valid() {
            tdutils::log_error!("Receive invalid {user_id:?}");
            return;
        }
        if self.get_user_force(env, user_id, "on_update_user_name") {
            self.on_update_user_name_impl(user_id, first_name, last_name);
            self.update_user(env, user_id, false, false);
        }
    }

    fn on_update_user_name_impl(
        &mut self,
        user_id: UserId,
        mut first_name: String,
        last_name: String,
    ) {
        let u = self.users.get_mut(&user_id.get()).unwrap();
        if first_name.is_empty() && last_name.is_empty() {
            first_name = u.phone_number.clone();
        }
        if u.first_name != first_name || u.last_name != last_name {
            u.first_name = first_name;
            u.last_name = last_name;
            u.is_name_changed = true;
            u.is_changed = true;
        }
    }

    /// `UserManager::on_update_user_usernames` (UserManager.cpp:3548-3577).
    pub fn on_update_user_usernames(
        &mut self,
        env: &mut UserEnv,
        user_id: UserId,
        usernames: Usernames,
    ) {
        if !user_id.is_valid() {
            tdutils::log_error!("Receive invalid {user_id:?}");
            return;
        }
        if self.get_user_force(env, user_id, "on_update_user_usernames") {
            self.on_update_user_usernames_impl(env, user_id, usernames);
            self.update_user(env, user_id, false, false);
        }
    }

    fn on_update_user_usernames_impl(
        &mut self,
        env: &mut UserEnv,
        user_id: UserId,
        usernames: Usernames,
    ) {
        let u = self.users.get_mut(&user_id.get()).unwrap();
        if u.usernames != usernames {
            // DialogManager/MessagesManager on_dialog_usernames_updated —
            // with those managers
            if u.can_be_edited_bot
                && u.usernames.get_editable_username() != usernames.get_editable_username()
            {
                u.is_full_info_changed = true;
            }
            u.usernames = usernames;
            u.is_username_changed = true;
            u.is_changed = true;
        } else if u.is_bot || !env.auth_is_bot {
            // on_dialog_usernames_received — with DialogManager
        }
    }

    /// `UserManager::on_update_user_phone_number`
    /// (UserManager.cpp:3579-3613).
    pub fn on_update_user_phone_number(
        &mut self,
        env: &mut UserEnv,
        user_id: UserId,
        phone_number: String,
    ) {
        if !user_id.is_valid() {
            tdutils::log_error!("Receive invalid {user_id:?}");
            return;
        }
        if self.get_user_force(env, user_id, "on_update_user_phone_number") {
            self.on_update_user_phone_number_impl(env, user_id, phone_number);
            self.update_user(env, user_id, false, false);
        }
    }

    fn on_update_user_phone_number_impl(
        &mut self,
        env: &mut UserEnv,
        user_id: UserId,
        phone_number: String,
    ) {
        if env.auth_is_bot {
            return;
        }
        let phone_number = clean_phone_number(phone_number);
        let u = self.users.get_mut(&user_id.get()).unwrap();
        if u.phone_number != phone_number {
            if !u.phone_number.is_empty()
                && self.resolved_phone_numbers.get(&u.phone_number) == Some(&user_id)
            {
                self.resolved_phone_numbers.remove(&u.phone_number);
            }
            u.phone_number = phone_number;
            u.is_phone_number_changed = true;
            u.is_changed = true;
        }
    }

    /// `UserManager::on_update_user_photo`/`do_update_user_photo`
    /// (UserManager.cpp:3615-3687) — the file ids are placeholders (module
    /// docs), so the bot `pending_user_photos_` branch and the
    /// `drop_user_photos` file-source handling are deferred.
    pub fn on_update_user_photo(
        &mut self,
        env: &mut UserEnv,
        user_id: UserId,
        photo: telegram_api::UserProfilePhoto,
        source: &str,
    ) {
        let new_photo = get_profile_photo(photo);
        self.do_update_user_photo(env, user_id, new_photo, true, source);
    }

    fn do_update_user_photo(
        &mut self,
        env: &mut UserEnv,
        user_id: UserId,
        new_photo: ProfilePhoto,
        invalidate_photo_cache: bool,
        source: &str,
    ) {
        let u = self.users.get_mut(&user_id.get()).unwrap();
        u.is_photo_inited = true;
        if need_update_profile_photo(&u.photo, &new_photo) {
            if u.access_hash == -1 && new_photo.small_file_id.is_valid() {
                tdutils::log_error!(
                    "Update profile photo of {user_id:?} without access hash from {source}"
                );
            }
            u.photo = new_photo;
            u.is_photo_changed = true;
            u.is_changed = true;
            if invalidate_photo_cache {
                // drop_user_photos — with the file-reference manager
            }
        } else if need_update_dialog_photo_minithumbnail(
            &u.photo.minithumbnail,
            &new_photo.minithumbnail,
        ) {
            u.photo.minithumbnail = new_photo.minithumbnail;
            u.is_photo_changed = true;
            u.is_changed = true;
        }
        let _ = (env, user_id, source);
    }

    /// `UserManager::on_update_user_colors` (UserManager.cpp:3739-3753).
    fn on_update_user_colors_impl(
        &mut self,
        user_id: UserId,
        mut accent_color_id: AccentColorId,
        background_custom_emoji_id: CustomEmojiId,
        peer_color_collectible: Option<PeerColorCollectible>,
    ) {
        if accent_color_id == AccentColorId::from_peer_id(user_id.get())
            || !accent_color_id.is_valid()
        {
            accent_color_id = AccentColorId::default();
        }
        let u = self.users.get_mut(&user_id.get()).unwrap();
        if u.accent_color_id != accent_color_id
            || u.background_custom_emoji_id != background_custom_emoji_id
            || u.peer_color_collectible != peer_color_collectible
        {
            u.accent_color_id = accent_color_id;
            u.background_custom_emoji_id = background_custom_emoji_id;
            u.peer_color_collectible = peer_color_collectible;
            u.is_accent_color_changed = true;
            u.is_changed = true;
        }
    }

    /// `UserManager::on_update_user_profile_colors`
    /// (UserManager.cpp:3755-3767).
    fn on_update_user_profile_colors_impl(
        &mut self,
        user_id: UserId,
        mut accent_color_id: AccentColorId,
        background_custom_emoji_id: CustomEmojiId,
    ) {
        if !accent_color_id.is_valid() {
            accent_color_id = AccentColorId::default();
        }
        let u = self.users.get_mut(&user_id.get()).unwrap();
        if u.profile_accent_color_id != accent_color_id
            || u.profile_background_custom_emoji_id != background_custom_emoji_id
        {
            u.profile_accent_color_id = accent_color_id;
            u.profile_background_custom_emoji_id = background_custom_emoji_id;
            u.is_accent_color_changed = true;
            u.is_changed = true;
        }
    }

    /// `UserManager::on_update_user_linked_community_id`
    /// (UserManager.cpp:3783-3791).
    fn on_update_user_linked_community_id_impl(
        &mut self,
        user_id: UserId,
        mut linked_community_id: CommunityId,
    ) {
        let is_bot = self
            .users
            .get(&user_id.get())
            .map(|u| u.is_bot && !u.is_deleted)
            .unwrap_or(false);
        if !linked_community_id.is_valid() || !is_bot {
            linked_community_id = CommunityId::default();
        }
        let u = self.users.get_mut(&user_id.get()).unwrap();
        if u.linked_community_id != linked_community_id {
            u.linked_community_id = linked_community_id;
            u.need_save_to_database = true;
        }
    }

    /// `UserManager::on_update_user_emoji_status`
    /// (UserManager.cpp:3793-3817).
    pub fn on_update_user_emoji_status(
        &mut self,
        env: &mut UserEnv,
        user_id: UserId,
        emoji_status: Option<EmojiStatus>,
    ) {
        if !user_id.is_valid() {
            tdutils::log_error!("Receive invalid {user_id:?}");
            return;
        }
        if self.get_user_force(env, user_id, "on_update_user_emoji_status") {
            self.on_update_user_emoji_status_impl(user_id, emoji_status);
            self.update_user(env, user_id, false, false);
        }
    }

    fn on_update_user_emoji_status_impl(
        &mut self,
        user_id: UserId,
        emoji_status: Option<EmojiStatus>,
    ) {
        let u = self.users.get_mut(&user_id.get()).unwrap();
        if u.emoji_status != emoji_status {
            u.emoji_status = emoji_status;
            u.is_emoji_status_changed = true;
            // effective emoji status might not be changed; checked in update_user
        }
    }

    /// `UserManager::on_update_user_story_ids_impl`
    /// (UserManager.cpp:3819-3899), reduced to the carried fields.
    pub fn on_update_user_story_ids(
        &mut self,
        user_id: UserId,
        has_live_story: bool,
        max_active_story_id: i32,
        max_read_story_id: StoryId,
    ) {
        let max_active_story_id = StoryId::new(max_active_story_id);
        if max_active_story_id != StoryId::default() && !max_active_story_id.is_server() {
            tdutils::log_error!(
                "Receive max active story {} for {user_id:?}",
                max_active_story_id.get()
            );
            return;
        }
        if max_read_story_id != StoryId::default() && !max_read_story_id.is_server() {
            tdutils::log_error!(
                "Receive max read story {} for {user_id:?}",
                max_read_story_id.get()
            );
            return;
        }
        if has_live_story && max_active_story_id == StoryId::default() {
            tdutils::log_error!("Receive live story without identifier for {user_id:?}");
            return;
        }

        let active_story_state = self.get_user_active_story_state(user_id);
        {
            let u = self.users.get_mut(&user_id.get()).unwrap();
            if u.has_live_story != has_live_story {
                u.has_live_story = has_live_story;
                u.need_save_to_database = true;
            }
            if u.max_active_story_id != max_active_story_id {
                u.max_active_story_id = max_active_story_id;
                u.need_save_to_database = true;
            }
        }
        if self.need_poll_user_active_stories(user_id) {
            let next_reload_time = tdutils::time::now() + MAX_ACTIVE_STORY_ID_RELOAD_TIME;
            let u = self.users.get_mut(&user_id.get()).unwrap();
            if next_reload_time
                > u.max_active_story_id_next_reload_time + MAX_ACTIVE_STORY_ID_RELOAD_TIME / 5.0
            {
                u.max_active_story_id_next_reload_time = next_reload_time;
                u.need_save_to_database = true;
            }
        }
        {
            let u = self.users.get_mut(&user_id.get()).unwrap();
            if !max_active_story_id.is_valid() {
                if u.max_read_story_id != StoryId::default() {
                    u.max_read_story_id = StoryId::default();
                    u.need_save_to_database = true;
                }
            } else if max_read_story_id.get() > u.max_read_story_id.get() {
                u.max_read_story_id = max_read_story_id;
                u.need_save_to_database = true;
            }
        }
        if active_story_state != self.get_user_active_story_state(user_id) {
            let u = self.users.get_mut(&user_id.get()).unwrap();
            u.is_changed = true;
        }
    }

    /// `UserManager::on_update_user_stories_hidden`
    /// (UserManager.cpp:3929-3955).
    pub fn on_update_user_stories_hidden(
        &mut self,
        env: &mut UserEnv,
        user_id: UserId,
        stories_hidden: bool,
    ) {
        if !user_id.is_valid() {
            tdutils::log_error!("Receive invalid {user_id:?}");
            return;
        }
        if self.get_user_force(env, user_id, "on_update_user_stories_hidden") {
            self.on_update_user_stories_hidden_impl(env, user_id, stories_hidden);
            self.update_user(env, user_id, false, false);
        }
    }

    fn on_update_user_stories_hidden_impl(
        &mut self,
        env: &mut UserEnv,
        user_id: UserId,
        stories_hidden: bool,
    ) {
        if env.auth_is_bot {
            return;
        }
        let u = self.users.get_mut(&user_id.get()).unwrap();
        if u.stories_hidden != stories_hidden {
            u.stories_hidden = stories_hidden;
            u.is_stories_hidden_changed = true;
            u.need_save_to_database = true;
        }
    }

    /// `UserManager::on_update_user_bot_verification_icon`
    /// (UserManager.cpp:3957-3962).
    fn on_update_user_bot_verification_icon_impl(
        &mut self,
        user_id: UserId,
        bot_verification_icon: CustomEmojiId,
    ) {
        let u = self.users.get_mut(&user_id.get()).unwrap();
        if u.bot_verification_icon != bot_verification_icon {
            u.bot_verification_icon = bot_verification_icon;
            u.is_changed = true;
        }
    }

    /// `UserManager::on_update_user_is_contact`
    /// (UserManager.cpp:3964-4000).
    fn on_update_user_is_contact_impl(
        &mut self,
        env: &mut UserEnv,
        user_id: UserId,
        is_contact: bool,
        mut is_mutual_contact: bool,
        mut is_close_friend: bool,
    ) {
        if env.auth_is_bot {
            return;
        }
        let my_id = self.get_my_id();
        if user_id == my_id {
            is_mutual_contact = is_contact;
            is_close_friend = false;
        }
        if !is_contact && (is_mutual_contact || is_close_friend) {
            tdutils::log_error!(
                "Receive is_mutual_contact = {is_mutual_contact}, and is_close_friend = {is_close_friend} for non-contact {user_id:?}"
            );
            is_mutual_contact = false;
            is_close_friend = false;
        }
        let u = self.users.get_mut(&user_id.get()).unwrap();
        if u.is_contact != is_contact
            || u.is_mutual_contact != is_mutual_contact
            || u.is_close_friend != is_close_friend
        {
            if u.is_contact != is_contact {
                u.is_contact = is_contact;
                u.is_is_contact_changed = true;
            }
            if u.is_mutual_contact != is_mutual_contact {
                u.is_mutual_contact = is_mutual_contact;
                u.is_is_mutual_contact_changed = true;
                // reload_contact_birthdates — with the Contacts step
            }
            u.is_close_friend = is_close_friend;
            u.is_changed = true;
        }
    }

    /// `UserManager::on_update_user_online`
    /// (UserManager.cpp:4002-4098).
    pub fn on_update_user_online(
        &mut self,
        env: &mut UserEnv,
        user_id: UserId,
        status: telegram_api::UserStatus,
    ) {
        if env.auth_is_bot {
            return;
        }
        if !user_id.is_valid() {
            tdutils::log_error!("Receive invalid {user_id:?}");
            return;
        }
        if self.get_user_force(env, user_id, "on_update_user_online") {
            let is_bot = self.users.get(&user_id.get()).unwrap().is_bot;
            if is_bot {
                tdutils::log_error!("Receive updateUserStatus about bot {user_id:?}");
                return;
            }
            self.on_update_user_online_impl(env, user_id, status);
            self.update_user(env, user_id, false, false);

            let was_online = self.users.get(&user_id.get()).unwrap().was_online;
            if user_id == self.my_id && self.was_online_remote != was_online {
                // only update was_online_remote from updateUserStatus
                self.was_online_remote = was_online;
                env.global
                    .binlog_pmc()
                    .set("my_was_online_remote", &self.was_online_remote.to_string());
            }
        }
    }

    fn on_update_user_online_impl(
        &mut self,
        env: &mut UserEnv,
        user_id: UserId,
        status: telegram_api::UserStatus,
    ) {
        if env.auth_is_bot {
            return;
        }
        let now = env.unix_time();
        let new_online;
        match status {
            telegram_api::UserStatus::userStatusOnline(st) => {
                new_online = st.expires;
                if new_online < now - 86400 {
                    tdutils::log_error!(
                        "Receive userStatusOnline expired more than one day in past {new_online}"
                    );
                }
            }
            telegram_api::UserStatus::userStatusOffline(st) => {
                let mut online = st.was_online;
                if online >= now {
                    online = now - 1;
                }
                new_online = online;
            }
            telegram_api::UserStatus::userStatusRecently(st) => {
                new_online = if st.by_me { -4 } else { -1 };
            }
            telegram_api::UserStatus::userStatusLastWeek(st) => {
                new_online = if st.by_me { -5 } else { -2 };
            }
            telegram_api::UserStatus::userStatusLastMonth(st) => {
                new_online = if st.by_me { -6 } else { -3 };
            }
            telegram_api::UserStatus::userStatusEmpty(_) => {
                new_online = 0;
            }
        }

        let old_online = self.users.get(&user_id.get()).unwrap().was_online;
        if new_online != old_online && !(new_online < 0 && user_id == self.get_my_id()) {
            let unix_time = env.unix_time();
            let old_is_online = old_online > unix_time;
            let new_is_online = new_online > unix_time;
            let u = self.users.get_mut(&user_id.get()).unwrap();
            u.was_online = new_online;
            u.is_status_changed = true;
            if u.was_online > 0 {
                u.local_was_online = 0;
            }
            if user_id == self.my_id {
                if self.my_was_online_local != 0 || old_is_online != new_is_online {
                    self.my_was_online_local = 0;
                    u.is_online_status_changed = true;
                }
                // OnlineManager::on_online_updated — with its step
            } else if old_is_online != new_is_online {
                u.is_online_status_changed = true;
            }
        }
    }

    /// `UserManager::on_update_user_local_was_online`
    /// (UserManager.cpp:4100-4141).
    pub fn on_update_user_local_was_online(
        &mut self,
        env: &mut UserEnv,
        user_id: UserId,
        local_was_online: i32,
    ) {
        if !user_id.is_valid() || env.auth_is_bot {
            return;
        }
        if !self.get_user_force(env, user_id, "on_update_user_local_was_online") {
            return;
        }
        {
            let unix_time = env.unix_time();
            let u = self.users.get_mut(&user_id.get()).unwrap();
            if u.is_deleted || u.is_bot || u.is_support || user_id == self.my_id {
                return;
            }
            if u.was_online > unix_time {
                // if user is currently online, ignore local online
                return;
            }
            // bring users online for 30 seconds
            let local_was_online = local_was_online + 30;
            if local_was_online < unix_time + 2
                || local_was_online <= u.local_was_online
                || local_was_online <= u.was_online
            {
                return;
            }
            let old_is_online = u.local_was_online > unix_time;
            u.local_was_online = local_was_online;
            u.is_status_changed = true;
            if !old_is_online {
                u.is_online_status_changed = true;
            }
        }
        self.update_user(env, user_id, false, false);
    }

    /// `UserManager::set_my_online_status` (UserManager.cpp:3077-3125).
    pub fn set_my_online_status(
        &mut self,
        env: &mut UserEnv,
        is_online: bool,
        send_update: bool,
        is_local: bool,
    ) {
        if env.auth_is_bot {
            return; // just in case
        }
        let my_id = self.get_my_id();
        if !self.get_user_force(env, my_id, "set_my_online_status") {
            return;
        }
        let unix_time = env.unix_time();
        let mut new_online = if is_online {
            unix_time + 300
        } else {
            unix_time - 1
        };

        let old_was_online = self.get_user_was_online_of(my_id, unix_time);
        {
            let u = self.users.get_mut(&my_id.get()).unwrap();
            if is_local {
                if !is_online {
                    new_online = new_online.min(u.was_online);
                }
                if new_online != self.my_was_online_local {
                    self.my_was_online_local = new_online;
                }
            } else if self.my_was_online_local != 0 || new_online != u.was_online {
                self.my_was_online_local = 0;
                u.was_online = new_online;
                u.need_save_to_database = true;
            }
        }
        if old_was_online != self.get_user_was_online_of(my_id, unix_time) {
            let u = self.users.get_mut(&my_id.get()).unwrap();
            u.is_status_changed = true;
            u.is_online_status_changed = true;
        }

        if self.was_online_local != new_online {
            self.was_online_local = new_online;
            env.global
                .binlog_pmc()
                .set("my_was_online_local", &self.was_online_local.to_string());
        }

        if send_update {
            self.update_user(env, my_id, false, false);
        }
    }

    /// `UserManager::get_user_was_online` (UserManager.cpp:5451-5467).
    pub fn get_user_was_online_of(&self, user_id: UserId, unix_time: i32) -> i32 {
        let Some(u) = self.get_user(user_id) else {
            return 0;
        };
        if u.is_deleted {
            return 0;
        }
        let mut was_online = u.was_online;
        if user_id == self.my_id {
            if self.my_was_online_local != 0 {
                was_online = self.my_was_online_local;
            }
        } else if u.local_was_online > 0
            && u.local_was_online > was_online
            && u.local_was_online > unix_time
        {
            was_online = u.local_was_online;
        }
        was_online
    }

    /// `UserManager::is_user_contact` (UserManager.cpp:5359-5361).
    pub fn is_user_contact_of(&self, user_id: UserId, is_mutual: bool) -> bool {
        match self.get_user(user_id) {
            Some(u) => {
                (if is_mutual {
                    u.is_mutual_contact
                } else {
                    u.is_contact
                }) && user_id != self.my_id
            }
            None => false,
        }
    }

    /// `UserManager::is_user_premium` (UserManager.cpp:5367-5369).
    pub fn is_user_premium_of(&self, user_id: UserId) -> bool {
        self.get_user(user_id)
            .map(|u| u.is_premium)
            .unwrap_or(false)
    }

    /// `UserManager::is_user_deleted` (UserManager.cpp:5375-5377).
    pub fn is_user_deleted_of(&self, user_id: UserId) -> bool {
        match self.get_user(user_id) {
            Some(u) => u.is_deleted,
            None => true,
        }
    }

    /// `UserManager::is_user_support` (UserManager.cpp:5383-5385).
    pub fn is_user_support_of(&self, user_id: UserId) -> bool {
        match self.get_user(user_id) {
            Some(u) => !u.is_deleted && u.is_support,
            None => false,
        }
    }

    /// `UserManager::is_user_bot` (UserManager.cpp:5391-5393).
    pub fn is_user_bot_of(&self, user_id: UserId) -> bool {
        match self.get_user(user_id) {
            Some(u) => !u.is_deleted && u.is_bot,
            None => false,
        }
    }

    /// `UserManager::have_input_peer_user` (UserManager.cpp:5271-5300);
    /// the user_messages branch lands with MessagesManager.
    pub fn have_input_peer_user(
        &self,
        env: &UserEnv,
        user_id: UserId,
        access_rights: AccessRights,
    ) -> bool {
        if user_id == self.my_id {
            return true;
        }
        let Some(u) = self.get_user(user_id) else {
            return env.auth_is_bot && user_id.is_valid();
        };
        if u.access_hash == -1 || u.is_min_access_hash {
            return env.auth_is_bot && user_id.is_valid();
        }
        match access_rights {
            AccessRights::Know | AccessRights::Read => true,
            AccessRights::Write => !u.is_deleted,
        }
    }

    /// `UserManager::is_user_online` (UserManager.cpp:5436-5442).
    pub fn is_user_online_of(
        &self,
        env: &UserEnv,
        user_id: UserId,
        tolerance: i32,
        unix_time: i32,
    ) -> bool {
        let unix_time = if unix_time <= 0 {
            env.unix_time()
        } else {
            unix_time
        };
        self.get_user_was_online_of(user_id, unix_time) > unix_time - tolerance
    }

    /// `UserManager::is_user_status_exact` (UserManager.cpp:5469-5472).
    pub fn is_user_status_exact_of(&self, user_id: UserId) -> bool {
        match self.get_user(user_id) {
            Some(u) => !u.is_deleted && !u.is_bot && u.was_online > 0,
            None => false,
        }
    }

    /// `UserManager::is_user_received_from_server` (UserManager.cpp:5474-5477).
    pub fn is_user_received_from_server_of(&self, user_id: UserId) -> bool {
        self.get_user(user_id)
            .map(|u| u.is_received_from_server)
            .unwrap_or(false)
    }

    /// `UserManager::get_user_search_text` (UserManager.cpp:5759-5762).
    pub fn get_user_search_text_of(&self, user_id: UserId) -> String {
        let Some(u) = self.get_user(user_id) else {
            return String::new();
        };
        format!(
            "{} {} {}",
            u.first_name,
            u.last_name,
            u.usernames.get_first_username()
        )
    }

    /// `UserManager::get_user_active_story_state` (UserManager.cpp:10257-10260).
    fn get_user_active_story_state(&self, user_id: UserId) -> (StoryId, StoryId, bool) {
        let u = self.users.get(&user_id.get()).unwrap();
        (u.max_active_story_id, u.max_read_story_id, u.has_live_story)
    }

    /// `UserManager::need_poll_user_active_stories`
    /// (UserManager.cpp:5691-5694); the StoryManager gate arrives with it.
    fn need_poll_user_active_stories(&self, user_id: UserId) -> bool {
        let (max_active, _max_read, _has_live) = self.get_user_active_story_state(user_id);
        max_active.is_server()
    }

    /// `UserManager::on_user_online_timeout` (UserManager.cpp:2857-2872).
    fn on_user_online_timeout(&mut self, env: &mut UserEnv, user_id: UserId) {
        if env.global.close_flag() {
            return;
        }
        if !self
            .users
            .get(&user_id.get())
            .map(|u| u.is_update_user_sent)
            .unwrap_or(false)
        {
            return;
        }
        let status = self.get_user_status_object(env, user_id);
        self.send_update(td_api::Update::updateUserStatus(td_api::updateUserStatus {
            user_id: user_id.get(),
            status,
        }));
        // DialogParticipantManager::update_user_online_member_count
    }

    /// Drive the online/emoji-status deadlines (the MultiTimeout
    /// replacement); the Td actor polls this on its tick.
    pub fn tick_timeouts(&mut self, env: &mut UserEnv) {
        let now = tdutils::time::now();
        let expired: Vec<i64> = self
            .user_online_timeouts
            .iter()
            .filter(|(_, deadline)| **deadline <= now)
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            self.user_online_timeouts.remove(&id);
            self.on_user_online_timeout(env, UserId::new(id));
        }
        let expired: Vec<i64> = self
            .user_emoji_status_timeouts
            .iter()
            .filter(|(_, deadline)| **deadline <= now)
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            self.user_emoji_status_timeouts.remove(&id);
            if self.users.contains_key(&id) {
                self.update_user(env, UserId::new(id), false, false);
            }
        }
    }

    /// The earliest armed deadline (the member MultiTimeouts' next firing
    /// time), or `None` when nothing is armed — the Td actor's re-arm key.
    pub(crate) fn next_deadline(&self) -> Option<f64> {
        self.user_online_timeouts
            .values()
            .chain(self.user_emoji_status_timeouts.values())
            .copied()
            .min_by(f64::total_cmp)
    }

    /// Test-only: arm a user-online deadline with the given past
    /// `was_online` — the td.rs actor-level sweep fixture (production
    /// deadlines are armed by `update_user` itself).
    #[cfg(test)]
    pub(crate) fn seed_online_deadline_for_test(
        &mut self,
        user_id: UserId,
        was_online: i32,
        deadline: f64,
    ) {
        // the default User mirrors the C++ User() constructor (a deleted
        // bot); the fixture is a regular cached user with a sent update
        let u = self.users.entry(user_id.get()).or_default();
        u.is_bot = false;
        u.is_deleted = false;
        u.is_update_user_sent = true;
        u.was_online = was_online;
        self.user_online_timeouts.insert(user_id.get(), deadline);
    }

    /// `UserManager::update_user` (UserManager.cpp:9872-10082). The
    /// cross-manager notifications are documented no-ops (module docs).
    pub fn update_user(
        &mut self,
        env: &mut UserEnv,
        user_id: UserId,
        from_binlog: bool,
        from_database: bool,
    ) {
        if self.users.get(&user_id.get()).unwrap().is_being_updated {
            tdutils::log_error!("Detected recursive update of {user_id:?}");
        }
        self.users.get_mut(&user_id.get()).unwrap().is_being_updated = true;
        self.update_user_impl(env, user_id, from_binlog, from_database);
        if let Some(u) = self.users.get_mut(&user_id.get()) {
            u.is_being_updated = false;
        }
    }

    fn update_user_impl(
        &mut self,
        env: &mut UserEnv,
        user_id: UserId,
        from_binlog: bool,
        from_database: bool,
    ) {
        if user_id == self.my_id {
            let is_premium = self.users.get(&user_id.get()).unwrap().is_premium;
            if env.options.get_option_boolean("is_premium", false) != is_premium {
                env.options.set_option_boolean("is_premium", is_premium);
                // ConfigManager::request_config / ReactionManager::
                // reload_reaction_list / MessagesManager::
                // update_is_translatable — with those steps
            }
        }
        let is_name_changed = self.users.get(&user_id.get()).unwrap().is_name_changed;
        let is_is_contact_changed = self
            .users
            .get(&user_id.get())
            .unwrap()
            .is_is_contact_changed;
        let is_username_changed = self.take_user_flag(user_id, UserFlag::Username);
        if is_name_changed || is_username_changed || is_is_contact_changed {
            // update_contacts_hints — with the Contacts step; the C++ also
            // clears is_username_changed here, unconditionally of which of
            // the three flags triggered the hints update
        }
        if self.take_user_flag(user_id, UserFlag::IsContact) {
            let is_contact = self.is_user_contact_of(user_id, false);
            if let Some(user_full) = self.users_full.get_mut(&user_id.get()) {
                if user_full.need_phone_number_privacy_exception && is_contact {
                    // on_update_user_full_need_phone_number_privacy_exception(false)
                    user_full.need_phone_number_privacy_exception = false;
                    user_full.is_changed = true;
                }
                user_full.need_send_update |= user_full.is_changed;
                user_full.need_save_to_database |= user_full.is_changed;
            }
            if self.users_full.contains_key(&user_id.get()) {
                self.update_user_full(env, user_id, "update_user");
            }
        }
        self.take_user_flag(user_id, UserFlag::IsMutualContact);
        // StoryManager::reload_dialog_expiring_stories — with its step
        if self.take_user_flag(user_id, UserFlag::IsDeleted) {
            let is_deleted = self.users.get(&user_id.get()).unwrap().is_deleted;
            if is_deleted && self.users_full.contains_key(&user_id.get()) {
                let u = self.users.get_mut(&user_id.get()).unwrap();
                u.is_full_info_changed = false;
                self.drop_user_full(env, user_id);
            }
        }
        self.take_user_flag(user_id, UserFlag::IsPremium);
        // StoryManager on_dialog_active_stories_order_updated — with its step
        self.take_user_flag(user_id, UserFlag::Name);
        // MessagesManager on_dialog_title_updated (+ secret chats)
        self.take_user_flag(user_id, UserFlag::Photo);
        // MessagesManager on_dialog_photo_updated (+ secret chats)
        self.take_user_flag(user_id, UserFlag::AccentColor);
        // MessagesManager on_dialog_accent_colors_updated (+ secret chats)
        if self.take_user_flag(user_id, UserFlag::PhoneNumber) {
            let phone_number = self.users.get(&user_id.get()).unwrap().phone_number.clone();
            if !phone_number.is_empty() && !env.auth_is_bot {
                self.resolved_phone_numbers.insert(phone_number, user_id);
            }
        }
        self.take_user_flag(user_id, UserFlag::HasBotForumView);
        // MessagesManager on_update_dialog_is_forum — with its step
        let unix_time = env.unix_time();
        let is_status_changed = self.users.get(&user_id.get()).unwrap().is_status_changed;
        if is_status_changed && user_id != self.my_id {
            let left_time =
                self.get_user_was_online_of(user_id, unix_time) as f64 - env.server_time();
            if (0.0..30.0 * 86400.0).contains(&left_time) {
                let left_time = left_time + 2.0; // to guarantee expiration
                self.user_online_timeouts
                    .insert(user_id.get(), tdutils::time::now() + left_time);
            } else {
                self.user_online_timeouts.remove(&user_id.get());
            }
        }
        self.take_user_flag(user_id, UserFlag::StoriesHidden);
        // StoryManager on_dialog_active_stories_order_updated — with its step
        if !env.auth_is_bot {
            let is_restricted = !self
                .users
                .get(&user_id.get())
                .unwrap()
                .restriction_reasons
                .is_empty();
            if is_restricted {
                self.restricted_user_ids.insert(user_id.get());
            } else {
                self.restricted_user_ids.remove(&user_id.get());
            }
        }

        // emoji status effective machine (UserManager.cpp:9996-10021)
        {
            let unix_time = env.unix_time();
            let (effective, last_sent) = {
                let u = self.users.get(&user_id.get()).unwrap();
                (
                    EmojiStatus::get_effective_emoji_status(
                        &u.emoji_status,
                        u.is_premium,
                        unix_time,
                    ),
                    u.last_sent_emoji_status.clone(),
                )
            };
            if effective != last_sent {
                if last_sent.is_some() {
                    self.user_emoji_status_timeouts.remove(&user_id.get());
                }
                let left_time;
                {
                    let u = self.users.get_mut(&user_id.get()).unwrap();
                    u.last_sent_emoji_status = effective.clone();
                    left_time = match &u.last_sent_emoji_status {
                        Some(status) => status.get_until_date() - unix_time,
                        None => -1,
                    };
                }
                if (0..30 * 86400).contains(&left_time) {
                    self.user_emoji_status_timeouts
                        .insert(user_id.get(), tdutils::time::now() + left_time as f64);
                }
                let u = self.users.get_mut(&user_id.get()).unwrap();
                u.is_changed = true;
                u.is_emoji_status_changed = false;
                // MessagesManager on_dialog_emoji_status_updated (+ secret chats)
            } else if self.take_user_flag(user_id, UserFlag::EmojiStatusChanged) {
                let u = self.users.get_mut(&user_id.get()).unwrap();
                u.need_save_to_database = true;
            }
        }

        // InlineQueriesManager::remove_recent_inline_bot — with its step
        if from_binlog || from_database {
            // DialogManager on_dialog_usernames_received — with its step
        }

        {
            let u = self.users.get_mut(&user_id.get()).unwrap();
            u.need_save_to_database |= u.is_changed;
            if u.need_save_to_database {
                if !from_database {
                    u.is_saved = false;
                }
                u.need_save_to_database = false;
            }
        }
        let is_changed = self.users.get(&user_id.get()).unwrap().is_changed;
        if is_changed {
            let user_object = self.get_user_object_of(env, user_id);
            self.send_update(td_api::Update::updateUser(td_api::updateUser {
                user: user_object,
            }));
            let u = self.users.get_mut(&user_id.get()).unwrap();
            u.is_changed = false;
            u.is_status_changed = false;
            u.is_update_user_sent = true;
        }
        let is_status_changed = self.users.get(&user_id.get()).unwrap().is_status_changed;
        if is_status_changed {
            if !from_database {
                let u = self.users.get_mut(&user_id.get()).unwrap();
                u.is_status_saved = false;
            }
            assert!(self.users.get(&user_id.get()).unwrap().is_update_user_sent);
            let status = self.get_user_status_object(env, user_id);
            self.send_update(td_api::Update::updateUserStatus(td_api::updateUserStatus {
                user_id: user_id.get(),
                status,
            }));
            let u = self.users.get_mut(&user_id.get()).unwrap();
            u.is_status_changed = false;
        }
        self.take_user_flag(user_id, UserFlag::OnlineStatus);
        // DialogParticipantManager::update_user_online_member_count

        if !from_database {
            self.save_user(env, user_id, from_binlog);
        }

        // The cache repair (`reload_user`, cpp:10065-10071) needs the
        // users.getUsers query — the net step.

        if self.take_user_flag(user_id, UserFlag::FullInfo)
            && self.users_full.contains_key(&user_id.get())
        {
            let user_full = self.users_full.get_mut(&user_id.get()).unwrap();
            user_full.need_send_update = true;
            self.update_user_full(env, user_id, "update_user is_full_info_changed");
            // reload_user_full — with the net step
        }
    }

    /// Read-and-clear one `User` change flag.
    fn take_user_flag(&mut self, user_id: UserId, flag: UserFlag) -> bool {
        let Some(u) = self.users.get_mut(&user_id.get()) else {
            return false;
        };
        let (value, field): (bool, &mut bool) = match flag {
            UserFlag::IsContact => {
                let v = u.is_is_contact_changed;
                (v, &mut u.is_is_contact_changed)
            }
            UserFlag::Username => {
                let v = u.is_username_changed;
                (v, &mut u.is_username_changed)
            }
            UserFlag::IsMutualContact => {
                let v = u.is_is_mutual_contact_changed;
                (v, &mut u.is_is_mutual_contact_changed)
            }
            UserFlag::IsDeleted => {
                let v = u.is_is_deleted_changed;
                (v, &mut u.is_is_deleted_changed)
            }
            UserFlag::IsPremium => {
                let v = u.is_is_premium_changed;
                (v, &mut u.is_is_premium_changed)
            }
            UserFlag::Name => {
                let v = u.is_name_changed;
                (v, &mut u.is_name_changed)
            }
            UserFlag::Photo => {
                let v = u.is_photo_changed;
                (v, &mut u.is_photo_changed)
            }
            UserFlag::AccentColor => {
                let v = u.is_accent_color_changed;
                (v, &mut u.is_accent_color_changed)
            }
            UserFlag::PhoneNumber => {
                let v = u.is_phone_number_changed;
                (v, &mut u.is_phone_number_changed)
            }
            UserFlag::HasBotForumView => {
                let v = u.is_has_bot_forum_view_changed;
                (v, &mut u.is_has_bot_forum_view_changed)
            }
            UserFlag::StoriesHidden => {
                let v = u.is_stories_hidden_changed;
                (v, &mut u.is_stories_hidden_changed)
            }
            UserFlag::EmojiStatusChanged => {
                let v = u.is_emoji_status_changed;
                (v, &mut u.is_emoji_status_changed)
            }
            UserFlag::FullInfo => {
                let v = u.is_full_info_changed;
                (v, &mut u.is_full_info_changed)
            }
            UserFlag::OnlineStatus => {
                let v = u.is_online_status_changed;
                (v, &mut u.is_online_status_changed)
            }
        };
        *field = false;
        value
    }

    /// `UserManager::update_user_full`
    /// (UserManager.cpp:10128-10225), the supported subset.
    pub fn update_user_full(&mut self, env: &mut UserEnv, user_id: UserId, source: &str) {
        if self
            .users_full
            .get(&user_id.get())
            .unwrap()
            .is_being_updated
        {
            tdutils::log_error!("Detected recursive update of full {user_id:?} from {source}");
        }
        self.users_full
            .get_mut(&user_id.get())
            .unwrap()
            .is_being_updated = true;
        self.update_user_full_impl(env, user_id, source);
        if let Some(user_full) = self.users_full.get_mut(&user_id.get()) {
            user_full.is_being_updated = false;
        }
    }

    fn update_user_full_impl(&mut self, env: &mut UserEnv, user_id: UserId, source: &str) {
        // unavailable_user_fulls_ — only populated by the net paths
        if let Some(user_full) = self.users_full.get_mut(&user_id.get()) {
            if user_full.is_common_chat_count_changed {
                user_full.is_common_chat_count_changed = false;
                // CommonDialogManager::drop_common_dialogs_cache — with
                // its step
            }
            // the star rating timeout (cpp:10144-10151) — with StarRating
            // the first-saved-music registration — with the Audios step
            // is_has_protected_content_changed — with MessagesManager
            // the file-source registration block — with the Files step
            user_full.need_send_update |= user_full.is_changed;
            user_full.need_save_to_database |= user_full.is_changed;
            user_full.is_changed = false;
        }
        let (need_send_update, need_save_to_database) = {
            let user_full = self.users_full.get(&user_id.get()).unwrap();
            (user_full.need_send_update, user_full.need_save_to_database)
        };
        if need_send_update {
            if !self
                .users_full
                .get(&user_id.get())
                .unwrap()
                .is_update_user_full_sent
            {
                tdutils::log_error!(
                    "Send partial updateUserFullInfo for {user_id:?} from {source}"
                );
                let user_full = self.users_full.get_mut(&user_id.get()).unwrap();
                user_full.is_update_user_full_sent = true;
            }
            let user_id_object = self.get_user_id_object(env, user_id, "updateUserFullInfo");
            let full_info = self.get_user_full_info_object_of(env, user_id);
            self.send_update(td_api::Update::updateUserFullInfo(
                td_api::updateUserFullInfo {
                    user_id: user_id_object,
                    user_full_info: full_info,
                },
            ));
            let user_full = self.users_full.get_mut(&user_id.get()).unwrap();
            user_full.need_send_update = false;
            // the my-birthdate suggested-action dismissal — with the
            // SuggestedActions step
        }
        if need_save_to_database {
            // save_user_full (UserManager.cpp:9345-9356) lands with the
            // Files-gated UserFull store
            let user_full = self.users_full.get_mut(&user_id.get()).unwrap();
            user_full.need_save_to_database = false;
        }
    }

    /// `UserManager::on_get_user_full` (UserManager.cpp:8975-9344) — the
    /// supported subset of the `userFull` fields (module docs).
    pub fn on_get_user_full(&mut self, env: &mut UserEnv, mut user_full: telegram_api::UserFull) {
        let user_id = UserId::new(user_full.id);
        if !user_id.is_valid() {
            tdutils::log_error!("Receive invalid {user_id:?}");
            return;
        }
        if !self.get_user_force(env, user_id, "on_get_user_full") {
            return;
        }

        let flags = user_full.flags;
        let flags2 = user_full.flags2;
        let user_full_expires_at = tdutils::time::now() + USER_FULL_EXPIRE_TIME;

        let is_blocked = flags & 1 != 0;
        let is_blocked_for_stories = flags & (1 << 27) != 0;

        if !self.users_full.contains_key(&user_id.get()) {
            self.users_full.entry(user_id.get()).or_insert_with(|| {
                let mut stored = UserFull::new();
                stored.expires_at = user_full_expires_at;
                stored
            });
        }

        let about = std::mem::take(&mut user_full.about);
        let can_be_called = flags & (1 << 4) != 0;
        let supports_video_calls = flags & (1 << 13) != 0;
        let has_private_calls = flags & (1 << 5) != 0;
        let can_pin_messages = flags & (1 << 7) != 0;
        let voice_messages_forbidden = flags & (1 << 20) != 0;
        let wallpaper_overridden = flags & (1 << 28) != 0;
        let read_dates_private = flags & (1 << 30) != 0;
        let contact_require_premium = flags & (1 << 29) != 0;
        let has_pinned_stories = flags & (1 << 26) != 0;
        let sponsored_enabled = flags2 & (1 << 7) != 0;
        let can_view_revenue = flags2 & (1 << 9) != 0;
        let can_manage_emoji_status = flags2 & (1 << 10) != 0;
        let noforwards_my_enabled = flags2 & (1 << 23) != 0;
        let noforwards_peer_enabled = flags2 & (1 << 24) != 0;
        let unofficial_security_risk = flags2 & (1 << 26) != 0;
        let gift_count = user_full.stargifts_count;
        let common_chat_count = user_full.common_chats_count;
        let personal_channel_id = if flags2 & (1 << 6) != 0 {
            user_full.personal_channel_id
        } else {
            0
        };
        let send_paid_message_stars = if flags2 & (1 << 14) != 0 {
            user_full.send_paid_messages_stars
        } else {
            0
        };
        let linked_community_id = 0i64; // the linked chat arrives via updates

        {
            let stored = self.users_full.get_mut(&user_id.get()).unwrap();
            // The photos/bot_info/business/stories parts of the telegram
            // object are handled by their subsystems (module docs); the
            // fields we carry are applied here.
            stored.expires_at = stored.expires_at.max(user_full_expires_at);
            if stored.is_blocked != is_blocked {
                stored.is_blocked = is_blocked;
                stored.is_changed = true;
            }
            if stored.is_blocked_for_stories != is_blocked_for_stories {
                stored.is_blocked_for_stories = is_blocked_for_stories;
                stored.is_changed = true;
            }
            if stored.about != about {
                stored.about = about;
                stored.is_changed = true;
            }
            if stored.can_be_called != can_be_called {
                stored.can_be_called = can_be_called;
                stored.is_changed = true;
            }
            if stored.supports_video_calls != supports_video_calls {
                stored.supports_video_calls = supports_video_calls;
                stored.is_changed = true;
            }
            if stored.has_private_calls != has_private_calls {
                stored.has_private_calls = has_private_calls;
                stored.is_changed = true;
            }
            if stored.can_pin_messages != can_pin_messages {
                stored.can_pin_messages = can_pin_messages;
                stored.is_changed = true;
            }
            if stored.voice_messages_forbidden != voice_messages_forbidden {
                stored.voice_messages_forbidden = voice_messages_forbidden;
                stored.is_changed = true;
            }
            if stored.has_pinned_stories != has_pinned_stories {
                stored.has_pinned_stories = has_pinned_stories;
                stored.is_changed = true;
            }
            if stored.wallpaper_overridden != wallpaper_overridden {
                stored.wallpaper_overridden = wallpaper_overridden;
                stored.is_changed = true;
            }
            if stored.read_dates_private != read_dates_private {
                stored.read_dates_private = read_dates_private;
                stored.is_changed = true;
            }
            if stored.contact_require_premium != contact_require_premium {
                stored.contact_require_premium = contact_require_premium;
                stored.is_changed = true;
            }
            if stored.sponsored_enabled != sponsored_enabled {
                stored.sponsored_enabled = sponsored_enabled;
                stored.is_changed = true;
            }
            if stored.can_view_revenue != can_view_revenue {
                stored.can_view_revenue = can_view_revenue;
                stored.is_changed = true;
            }
            if stored.can_manage_emoji_status != can_manage_emoji_status {
                stored.can_manage_emoji_status = can_manage_emoji_status;
                stored.is_changed = true;
            }
            if stored.noforwards_my_enabled != noforwards_my_enabled
                || stored.noforwards_peer_enabled != noforwards_peer_enabled
            {
                stored.noforwards_my_enabled = noforwards_my_enabled;
                stored.noforwards_peer_enabled = noforwards_peer_enabled;
                stored.is_changed = true;
            }
            if stored.unofficial_security_risk != unofficial_security_risk {
                stored.unofficial_security_risk = unofficial_security_risk;
                stored.is_changed = true;
            }
            if stored.gift_count != gift_count {
                stored.gift_count = gift_count;
                stored.is_changed = true;
            }
            if stored.common_chat_count != common_chat_count {
                stored.common_chat_count = common_chat_count;
                stored.is_common_chat_count_changed = true;
                stored.is_changed = true;
            }
            if stored.personal_channel_id != personal_channel_id {
                stored.personal_channel_id = personal_channel_id;
                stored.is_changed = true;
            }
            if stored.send_paid_message_stars != send_paid_message_stars {
                stored.send_paid_message_stars = send_paid_message_stars;
                stored.is_changed = true;
            }
            if stored.linked_community_id.get() != linked_community_id {
                stored.linked_community_id = CommunityId::new(linked_community_id);
                stored.is_changed = true;
            }
        }
        self.update_user_full(env, user_id, "on_get_user_full");
    }

    /// `UserManager::invalidate_user_full` (UserManager.cpp:4809-4823).
    pub fn invalidate_user_full(&mut self, env: &mut UserEnv, user_id: UserId) {
        if self
            .users_full
            .get(&user_id.get())
            .map(|user_full| !user_full.is_being_updated)
            .unwrap_or(false)
        {
            let stored = self.users_full.get_mut(&user_id.get()).unwrap();
            stored.expires_at = 0.0;
            stored.is_changed = true;
            self.update_user_full(env, user_id, "invalidate_user_full");
        }
    }

    /// `UserManager::drop_user_full` (UserManager.cpp:9552-9602), without
    /// the photo/file-source handling.
    pub fn drop_user_full(&mut self, env: &mut UserEnv, user_id: UserId) {
        self.users_full.remove(&user_id.get());
        // the kv erase follows the store, which is Files-gated
        let _ = env;
    }

    /// `UserManager::add_user_full` (UserManager.cpp:8898-8902).
    pub fn add_user_full(&mut self, user_id: UserId) -> &mut UserFull {
        assert!(user_id.is_valid());
        self.users_full.entry(user_id.get()).or_default()
    }

    /// `UserManager::get_user_status_object` (UserManager.cpp:10227-10255).
    pub fn get_user_status_object(&self, env: &UserEnv, user_id: UserId) -> td_api::UserStatus {
        let u = self.users.get(&user_id.get()).unwrap();
        if u.is_bot {
            return td_api::UserStatus::userStatusOnline(td_api::userStatusOnline {
                expires: i32::MAX,
            });
        }

        let was_online = self.get_user_was_online_of(user_id, env.unix_time());
        match was_online {
            -6 | -3 => td_api::UserStatus::userStatusLastMonth(td_api::userStatusLastMonth {
                by_my_privacy_settings: was_online == -6,
            }),
            -5 | -2 => td_api::UserStatus::userStatusLastWeek(td_api::userStatusLastWeek {
                by_my_privacy_settings: was_online == -5,
            }),
            -4 | -1 => td_api::UserStatus::userStatusRecently(td_api::userStatusRecently {
                by_my_privacy_settings: was_online == -4,
            }),
            0 => td_api::UserStatus::userStatusEmpty(td_api::userStatusEmpty),
            _ => {
                let time = env.unix_time();
                if was_online > time {
                    td_api::UserStatus::userStatusOnline(td_api::userStatusOnline {
                        expires: was_online,
                    })
                } else {
                    td_api::UserStatus::userStatusOffline(td_api::userStatusOffline { was_online })
                }
            }
        }
    }

    /// `UserManager::get_update_unknown_user_object`
    /// (UserManager.cpp:10269-10276).
    pub fn get_update_unknown_user_object(&self, user_id: UserId) -> td_api::Update {
        let user = make_unknown_user_object(user_id, self.my_id);
        td_api::Update::updateUser(td_api::updateUser { user })
    }

    /// `UserManager::get_user_id_object` (UserManager.cpp:10278-10291).
    pub fn get_user_id_object(&mut self, env: &mut UserEnv, user_id: UserId, source: &str) -> i64 {
        let _ = env;
        if user_id.is_valid()
            && self.get_user(user_id).is_none()
            && !self.unknown_users.contains(&user_id.get())
        {
            tdutils::log_error!("Have no information about {user_id:?} from {source}");
            self.unknown_users.insert(user_id.get());
            let update = self.get_update_unknown_user_object(user_id);
            self.send_update(update);
            // the unknown-self get_me query — with the net step
        }
        user_id.get()
    }

    /// `UserManager::get_user_object(UserId, const User *)`
    /// (UserManager.cpp:10301-10336).
    pub fn get_user_object_of(&self, env: &UserEnv, user_id: UserId) -> td_api::User {
        let Some(u) = self.get_user(user_id) else {
            return make_unknown_user_object(user_id, self.my_id);
        };
        let r#type = if u.is_deleted {
            td_api::UserType::userTypeDeleted(td_api::userTypeDeleted)
        } else if u.is_bot {
            td_api::UserType::userTypeBot(td_api::userTypeBot {
                can_be_edited: u.can_be_edited_bot,
                can_join_groups: u.can_join_groups,
                can_read_all_group_messages: u.can_read_all_group_messages,
                has_main_web_app: u.has_main_app,
                has_topics: u.has_bot_forum_view,
                allows_users_to_create_topics: u.has_bot_forum_view && !u.can_bot_create_topics,
                can_manage_bots: u.can_manage_bots,
                is_inline: u.is_inline_bot,
                inline_query_placeholder: u.inline_query_placeholder.clone(),
                supports_guest_queries: u.is_guestchat_bot,
                is_guard: u.is_guard_bot,
                need_location: u.need_location_bot,
                can_connect_to_business: u.is_business_bot,
                can_be_added_to_attachment_menu: u.can_be_added_to_attach_menu,
                active_user_count: u.bot_active_users,
            })
        } else {
            td_api::UserType::userTypeRegular(td_api::userTypeRegular)
        };

        let emoji_status = match &u.last_sent_emoji_status {
            Some(status) if status.get_custom_emoji_id().is_valid() => td_api::EmojiStatus {
                r#type: td_api::EmojiStatusType::emojiStatusTypeCustomEmoji(
                    td_api::emojiStatusTypeCustomEmoji {
                        custom_emoji_id: status.get_custom_emoji_id().get(),
                    },
                ),
                expiration_date: status.get_until_date(),
            },
            // collectible emoji statuses carry no custom emoji id in the
            // reduced payload; the upgraded-gift form lands with the
            // Gifts step (the C++ emits emojiStatusUpgradedGift here)
            _ => td_api::EmojiStatus {
                r#type: td_api::EmojiStatusType::emojiStatusTypeCustomEmoji(
                    td_api::emojiStatusTypeCustomEmoji { custom_emoji_id: 0 },
                ),
                expiration_date: 0,
            },
        };
        let restricts_new_chats = u.contact_require_premium && !u.is_mutual_contact;
        let have_access = self.have_input_peer_user(env, user_id, AccessRights::Know);
        td_api::User {
            id: user_id.get(),
            first_name: u.first_name.clone(),
            last_name: u.last_name.clone(),
            usernames: u.usernames.get_usernames_object(),
            phone_number: u.phone_number.clone(),
            status: self.get_user_status_object(env, user_id),
            // profile_photo: null in the C++ until the Files step — the
            // codegen forces an empty payload here (module docs)
            profile_photo: empty_profile_photo_object(),
            accent_color_id: {
                // ThemeManager::get_accent_color_id_object(accent, default):
                // invalid ids fall back to the id-derived default
                if u.accent_color_id.is_valid() {
                    u.accent_color_id.get()
                } else {
                    AccentColorId::from_peer_id(user_id.get()).get()
                }
            },
            background_custom_emoji_id: u.background_custom_emoji_id.get(),
            upgraded_gift_colors: u
                .peer_color_collectible
                .as_ref()
                .map(|collectible| collectible.get_upgraded_gift_colors_object())
                .unwrap_or_default(),
            profile_accent_color_id: u.profile_accent_color_id.get(),
            profile_background_custom_emoji_id: u.profile_background_custom_emoji_id.get(),
            emoji_status,
            is_contact: self.is_user_contact_of(user_id, false),
            is_mutual_contact: self.is_user_contact_of(user_id, true),
            is_close_friend: u.is_close_friend,
            verification_status: td_api::VerificationStatus {
                is_verified: u.is_verified,
                is_scam: u.is_scam,
                is_fake: u.is_fake,
                bot_verification_icon_custom_emoji_id: u.bot_verification_icon.get(),
            },
            is_premium: u.is_premium,
            is_support: u.is_support,
            restriction_info: get_restriction_info_object(
                &u.restriction_reasons,
                &env.restriction_options(),
            )
            .unwrap_or(td_api::RestrictionInfo {
                restriction_reason: String::new(),
                has_sensitive_content: false,
            }),
            active_story_state: {
                let (max_active, max_read, has_live) = self.get_user_active_story_state(user_id);
                if !max_active.is_server() {
                    // the C++ emits null; the forced stand-in is the read state
                    td_api::ActiveStoryState::activeStoryStateRead(td_api::activeStoryStateRead)
                } else if has_live {
                    td_api::ActiveStoryState::activeStoryStateLive(td_api::activeStoryStateLive {
                        story_id: max_active.get(),
                    })
                } else if max_active.get() > max_read.get() {
                    td_api::ActiveStoryState::activeStoryStateUnread(td_api::activeStoryStateUnread)
                } else {
                    td_api::ActiveStoryState::activeStoryStateRead(td_api::activeStoryStateRead)
                }
            },
            restricts_new_chats,
            paid_message_star_count: u.paid_message_star_count,
            have_access,
            r#type,
            language_code: u.language_code.clone(),
            added_to_attachment_menu: u.attach_menu_enabled,
        }
    }

    /// `UserManager::get_users_object` (UserManager.cpp:10342-10348).
    pub fn get_users_object_of(
        &mut self,
        env: &mut UserEnv,
        total_count: i32,
        user_ids: &[UserId],
    ) -> td_api::Users {
        let total_count = if total_count == -1 {
            user_ids.len() as i32
        } else {
            total_count
        };
        let mut user_id_objects = Vec::with_capacity(user_ids.len());
        for user_id in user_ids {
            user_id_objects.push(self.get_user_id_object(env, *user_id, "get_users_object"));
        }
        td_api::Users {
            total_count,
            user_ids: user_id_objects,
        }
    }

    /// `UserManager::get_user_full_info_object`
    /// (UserManager.cpp:10354-10463), the supported subset.
    pub fn get_user_full_info_object_of(
        &self,
        env: &UserEnv,
        user_id: UserId,
    ) -> td_api::UserFullInfo {
        let user_full = self
            .users_full
            .get(&user_id.get())
            .expect("get_user_full_info_object needs a loaded user full");
        let is_bot = self.is_user_bot_of(user_id);
        let is_premium = self.is_user_premium_of(user_id);
        let voice_messages_forbidden = is_premium && user_full.voice_messages_forbidden;
        // bio: the C++ runs find_entities for non-bots — the MessageEntity
        // step; bots carry the about in bot_info (empty here, same step).
        let bio = td_api::FormattedText {
            text: if is_bot {
                String::new()
            } else {
                user_full.about.clone()
            },
            entities: Vec::new(),
        };
        let _ = env;
        td_api::UserFullInfo {
            // photos: forced empty until the Files step (module docs)
            personal_photo: td_api::ChatPhoto::default(),
            photo: td_api::ChatPhoto::default(),
            public_photo: td_api::ChatPhoto::default(),
            community_id: user_full.linked_community_id.get(),
            block_list: if user_full.is_blocked {
                td_api::BlockList::blockListMain(td_api::blockListMain)
            } else if user_full.is_blocked_for_stories {
                td_api::BlockList::blockListStories(td_api::blockListStories)
            } else {
                // the C++ emits null; the forced stand-in is blockListMain
                td_api::BlockList::blockListMain(td_api::blockListMain)
            },
            can_be_called: user_full.can_be_called,
            supports_video_calls: user_full.supports_video_calls,
            has_private_calls: user_full.has_private_calls,
            has_private_forwards: !user_full.private_forward_name.is_empty(),
            has_restricted_voice_and_video_note_messages: voice_messages_forbidden,
            has_posted_to_profile_stories: user_full.has_pinned_stories,
            has_sponsored_messages_enabled: user_full.sponsored_enabled,
            need_phone_number_privacy_exception: user_full.need_phone_number_privacy_exception,
            set_chat_background: user_full.wallpaper_overridden,
            uses_unofficial_app: user_full.unofficial_security_risk,
            bio,
            birthdate: td_api::Birthdate {
                day: user_full.birthdate.get_day(),
                month: user_full.birthdate.get_month(),
                year: user_full.birthdate.get_year(),
            },
            personal_chat_id: user_full.personal_channel_id,
            gift_count: user_full.gift_count,
            group_in_common_count: user_full.common_chat_count,
            incoming_paid_message_star_count: user_full.charge_paid_message_stars,
            outgoing_paid_message_star_count: user_full.send_paid_message_stars,
            gift_settings: td_api::GiftSettings {
                show_gift_button: false,
                accepted_gift_types: td_api::AcceptedGiftTypes::default(),
            },
            bot_verification: td_api::BotVerification {
                bot_user_id: 0,
                icon_custom_emoji_id: 0,
                custom_description: td_api::FormattedText {
                    text: String::new(),
                    entities: Vec::new(),
                },
            },
            main_profile_tab: td_api::ProfileTab::profileTabPosts(td_api::profileTabPosts),
            first_profile_audio: td_api::Audio::default(),
            rating: td_api::UserRating {
                level: 0,
                is_maximum_level_reached: false,
                rating: 0,
                ..td_api::UserRating::default()
            },
            pending_rating: td_api::UserRating {
                level: 0,
                is_maximum_level_reached: false,
                rating: 0,
                ..td_api::UserRating::default()
            },
            pending_rating_date: 0,
            note: td_api::FormattedText {
                text: String::new(),
                entities: Vec::new(),
            },
            business_info: td_api::BusinessInfo::default(),
            bot_info: td_api::BotInfo::default(),
        }
    }

    /// `UserManager::get_me` (UserManager.cpp:5148-5157) — the local fast
    /// path; the users.getUsers query stays with the net step.
    pub fn get_me(&mut self, env: &mut UserEnv) -> UserId {
        let my_id = self.my_id;
        if !self.get_user_force(env, my_id, "get_me") {
            return UserId::default();
        }
        my_id
    }

    /// `UserManager::get_user` (UserManager.cpp:5159-5192) — the local
    /// path with the synchronous database preload; the merged
    /// users.getUsers query stays with the net step. Returns whether the
    /// user is answerable locally.
    pub fn get_user_local(&mut self, env: &mut UserEnv, user_id: UserId) -> bool {
        if !user_id.is_valid() {
            return false;
        }
        if Self::is_special_user_id(user_id, env.global.is_test_dc()) {
            self.get_user_force(env, user_id, "get_user");
        }
        if env.auth_is_bot {
            self.have_accessible_user(user_id)
        } else {
            self.have_min_user(user_id)
        }
    }

    /// `UserManager::get_current_state`
    /// (UserManager.cpp:10533-10566) — the supported blocks.
    pub fn get_current_state(&mut self, env: &mut UserEnv, updates: &mut Vec<td_api::Update>) {
        let unknown_users: Vec<i64> = self.unknown_users.iter().copied().collect();
        for user_id in unknown_users {
            if !self.have_min_user(UserId::new(user_id)) {
                updates.push(self.get_update_unknown_user_object(UserId::new(user_id)));
            }
        }
        let unknown_secret_chats: Vec<i32> = self.unknown_secret_chats.iter().copied().collect();
        for secret_chat_id in unknown_secret_chats {
            if !self.secret_chats.contains_key(&secret_chat_id) {
                updates.push(td_api::Update::updateSecretChat(td_api::updateSecretChat {
                    secret_chat: make_unknown_secret_chat_object(SecretChatId::new(secret_chat_id)),
                }));
            }
        }

        let user_ids: Vec<i64> = self.users.keys().copied().collect();
        for user_id in user_ids {
            let user_object = self.get_user_object_of(env, UserId::new(user_id));
            updates.push(td_api::Update::updateUser(td_api::updateUser {
                user: user_object,
            }));
        }
        // secret chat objects contain user_id, so they must be sent after users
        let secret_chat_ids: Vec<i32> = self.secret_chats.keys().copied().collect();
        for secret_chat_id in secret_chat_ids {
            let object = make_secret_chat_object(
                SecretChatId::new(secret_chat_id),
                &self.secret_chats[&secret_chat_id],
            );
            updates.push(td_api::Update::updateSecretChat(td_api::updateSecretChat {
                secret_chat: object,
            }));
        }
        // users_full blocks and the contact birthdates/freeze-state
        // updates arrive with the UserFull store and the Contacts step
    }

    // The read-projection helpers keep the on_get_user control flow flat.
    fn u_is_received(&self, user_id: UserId) -> bool {
        self.users
            .get(&user_id.get())
            .map(|u| u.is_received)
            .unwrap_or(false)
    }
    fn u_need_apply_min_photo(&self, user_id: UserId) -> bool {
        self.users
            .get(&user_id.get())
            .map(|u| u.need_apply_min_photo)
            .unwrap_or(false)
    }
    fn u_is_deleted(&self, user_id: UserId) -> bool {
        self.users
            .get(&user_id.get())
            .map(|u| u.is_deleted)
            .unwrap_or(true)
    }
    fn u_was_online(&self, user_id: UserId) -> i32 {
        self.users
            .get(&user_id.get())
            .map(|u| u.was_online)
            .unwrap_or(0)
    }
}

/// `UserManager::get_update_unknown_secret_chat_object`
/// (UserManager.cpp:10496-10500).
fn make_unknown_secret_chat_object(secret_chat_id: SecretChatId) -> td_api::SecretChat {
    td_api::SecretChat {
        id: secret_chat_id.get(),
        user_id: 0,
        state: td_api::SecretChatState::secretChatStateClosed(td_api::secretChatStateClosed),
        is_outbound: false,
        key_hash: Vec::new(),
        layer: 0,
    }
}

/// `UserManager::get_secret_chat_object_const`
/// (UserManager.cpp:10525-10531).
fn make_secret_chat_object(
    secret_chat_id: SecretChatId,
    secret_chat: &SecretChat,
) -> td_api::SecretChat {
    td_api::SecretChat {
        id: secret_chat_id.get(),
        user_id: secret_chat.user_id.get(),
        state: match secret_chat.state {
            SecretChatState::Waiting => {
                td_api::SecretChatState::secretChatStatePending(td_api::secretChatStatePending)
            }
            SecretChatState::Active => {
                td_api::SecretChatState::secretChatStateReady(td_api::secretChatStateReady)
            }
            SecretChatState::Closed | SecretChatState::Unknown => {
                td_api::SecretChatState::secretChatStateClosed(td_api::secretChatStateClosed)
            }
        },
        is_outbound: secret_chat.is_outbound,
        key_hash: secret_chat.key_hash.clone().into_bytes(),
        layer: secret_chat.layer,
    }
}

/// The forced-empty `profilePhoto` payload (the codegen cannot express
/// null; module docs).
fn empty_profile_photo_object() -> td_api::ProfilePhoto {
    td_api::ProfilePhoto {
        id: 0,
        small: td_api::File::default(),
        big: td_api::File::default(),
        minithumbnail: td_api::Minithumbnail {
            width: 0,
            height: 0,
            data: Vec::new(),
        },
        has_animation: false,
        is_personal: false,
    }
}

/// `UserManager::get_update_unknown_user_object`'s payload
/// (UserManager.cpp:10269-10276).
fn make_unknown_user_object(user_id: UserId, my_id: UserId) -> td_api::User {
    let have_access = user_id == my_id;
    td_api::User {
        id: user_id.get(),
        first_name: String::new(),
        last_name: String::new(),
        usernames: td_api::Usernames {
            active_usernames: Vec::new(),
            disabled_usernames: Vec::new(),
            editable_username: String::new(),
            collectible_usernames: Vec::new(),
        },
        phone_number: String::new(),
        status: td_api::UserStatus::userStatusEmpty(td_api::userStatusEmpty),
        profile_photo: empty_profile_photo_object(),
        accent_color_id: AccentColorId::from_peer_id(user_id.get()).get(),
        background_custom_emoji_id: 0,
        upgraded_gift_colors: td_api::UpgradedGiftColors::default(),
        profile_accent_color_id: -1,
        profile_background_custom_emoji_id: 0,
        emoji_status: td_api::EmojiStatus {
            r#type: td_api::EmojiStatusType::emojiStatusTypeCustomEmoji(
                td_api::emojiStatusTypeCustomEmoji { custom_emoji_id: 0 },
            ),
            expiration_date: 0,
        },
        is_contact: false,
        is_mutual_contact: false,
        is_close_friend: false,
        verification_status: td_api::VerificationStatus {
            is_verified: false,
            is_scam: false,
            is_fake: false,
            bot_verification_icon_custom_emoji_id: 0,
        },
        is_premium: false,
        is_support: false,
        restriction_info: td_api::RestrictionInfo {
            restriction_reason: String::new(),
            has_sensitive_content: false,
        },
        active_story_state: td_api::ActiveStoryState::activeStoryStateRead(
            td_api::activeStoryStateRead,
        ),
        restricts_new_chats: false,
        paid_message_star_count: 0,
        have_access,
        r#type: td_api::UserType::userTypeUnknown(td_api::userTypeUnknown),
        language_code: String::new(),
        added_to_attachment_menu: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_env() -> (TempEnv, UserManager) {
        TempEnv::new("unit")
    }

    /// A Global + OptionManager pair over a throwaway database.
    struct TempEnv {
        global: Global,
        options: OptionManager,
        dir: String,
    }

    impl TempEnv {
        fn new(name: &str) -> (TempEnv, UserManager) {
            // Unique per test thread: parallel tests in one process would
            // otherwise flock the same binlog against each other.
            let dir = std::env::temp_dir()
                .join(format!(
                    "td-rs-m3-{name}-{}-{:?}",
                    std::process::id(),
                    std::thread::current().id()
                ))
                .to_string_lossy()
                .into_owned();
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let opened = tddb::td_db::TdDb::open(tddb::td_db::Parameters {
                database_directory: dir.clone(),
                ..Default::default()
            })
            .unwrap();
            let mut global = Global::new();
            global.init(opened.database);
            // Mark the database authorized: `TdDb::open` destroys the
            // sqlite file of a never-authorized database (the C++
            // "Destroy SQLite database, because wasn't authorized yet").
            global.binlog_pmc().set("auth", "ok");
            let options = OptionManager::new(global.is_test_dc());
            (
                TempEnv {
                    global,
                    options,
                    dir,
                },
                UserManager::new(),
            )
        }

        fn env(&mut self, auth_is_bot: bool) -> UserEnv<'_> {
            UserEnv {
                global: &mut self.global,
                options: &mut self.options,
                auth_is_bot,
            }
        }
    }

    impl Drop for TempEnv {
        fn drop(&mut self) {
            self.global.close_all(false);
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn telegram_user(id: i64, first_name: &str, flags: i32) -> telegram_api::user {
        telegram_api::user {
            flags,
            access_hash: 123,
            id,
            first_name: first_name.to_owned(),
            ..telegram_api::user::default()
        }
    }

    #[test]
    fn on_get_user_creates_and_updates_the_cache() {
        let (mut temp, mut um) = test_env();

        // A non-received (min) user first: cache-only application.
        let mut min_user = telegram_user(5, "Min", USER_ACCESS_HASH_MASK | (1 << 1)); // access_hash, first_name
        min_user.min = true;
        let user_id = um.on_get_user(
            &mut temp.env(false),
            telegram_api::User::user(min_user),
            "test",
        );
        assert_eq!(user_id.get(), 5);
        let u = um.get_user(user_id).unwrap();
        assert_eq!(u.first_name, "Min");
        assert_eq!(u.access_hash, 123);
        assert!(u.is_min_access_hash, "min user without a phone");
        assert!(!u.is_received);
        assert!(!u.is_deleted, "deleted is an explicit telegram flag");
        // `update_user` at the end of `on_get_user` consumes the flag
        // (is_changed starts true on a new user; the update flushes it).
        assert!(!u.is_changed);

        // The full user arrives: is_received flips and the update fires.
        let full_flags = USER_ACCESS_HASH_MASK | (1 << 1); // access_hash, first_name
        let user_id = um.on_get_user(
            &mut temp.env(false),
            telegram_api::User::user(telegram_user(5, "Alice", full_flags)),
            "test",
        );
        let u = um.get_user(user_id).unwrap();
        assert_eq!(u.first_name, "Alice");
        assert!(u.is_received);
        assert!(!u.is_deleted);
        assert_eq!(u.cache_version, USER_CACHE_VERSION);
        let updates = um.take_updates();
        assert!(
            updates
                .iter()
                .any(|update| matches!(update, td_api::Update::updateUser(u) if u.user.id == 5 && u.user.first_name == "Alice")),
            "updateUser must be sent"
        );
    }

    #[test]
    fn on_get_user_self_sets_my_id_and_phone_option() {
        let (mut temp, mut um) = test_env();
        let mut user = telegram_user(
            77,
            "Me",
            USER_ACCESS_HASH_MASK | (1 << 1) | USER_PHONE_MASK, // access_hash, first_name, phone
        );
        user.phone = "+15550001111".to_owned();
        user.self_ = true;
        um.on_get_user(&mut temp.env(false), telegram_api::User::user(user), "test");
        assert_eq!(um.get_my_id().get(), 77);
        // the phone option is set through the pending option updates
        assert!(!temp.options.take_updates().is_empty());
    }

    #[test]
    fn min_user_does_not_override_the_received_access_hash() {
        let (mut temp, mut um) = test_env();
        let full_flags = USER_ACCESS_HASH_MASK | (1 << 1);
        um.on_get_user(
            &mut temp.env(false),
            telegram_api::User::user(telegram_user(5, "A", full_flags)),
            "t",
        );
        // a min user with a different access hash must not override it
        let mut min_user = telegram_user(5, "A", USER_ACCESS_HASH_MASK | (1 << 1));
        min_user.min = true;
        min_user.access_hash = 999;
        um.on_get_user(
            &mut temp.env(false),
            telegram_api::User::user(min_user),
            "t",
        );
        assert_eq!(um.get_user(UserId::new(5)).unwrap().access_hash, 123);
        assert!(!um.get_user(UserId::new(5)).unwrap().is_min_access_hash);
    }

    #[test]
    fn self_as_frozen_min_user_is_ignored() {
        let (mut temp, mut um) = test_env();
        let mut me = telegram_user(77, "Me", USER_ACCESS_HASH_MASK | (1 << 1));
        me.self_ = true;
        um.on_get_user(&mut temp.env(false), telegram_api::User::user(me), "t");
        // deleted min self: ignored
        let mut frozen = telegram_user(77, "Me", USER_ACCESS_HASH_MASK | (1 << 1));
        frozen.self_ = true;
        frozen.min = true;
        frozen.deleted = true;
        um.on_get_user(&mut temp.env(false), telegram_api::User::user(frozen), "t");
        let u = um.get_user(UserId::new(77)).unwrap();
        assert!(!u.is_deleted, "the frozen-min update must be ignored");
    }

    #[test]
    fn user_status_object_matches_the_cpp_table() {
        let (mut temp, mut um) = test_env();
        let env = temp.env(false);
        let make = |um: &mut UserManager, was_online: i32| {
            um.add_user(UserId::new(9));
            um.users.get_mut(&9).unwrap().is_bot = false;
            um.users.get_mut(&9).unwrap().is_deleted = false;
            um.users.get_mut(&9).unwrap().was_online = was_online;
            um.get_user_status_object(&env, UserId::new(9))
        };
        // a bot is always online with the i32::MAX expiry
        um.add_user(UserId::new(10));
        um.users.get_mut(&10).unwrap().is_bot = true;
        assert!(matches!(
            um.get_user_status_object(&env, UserId::new(10)),
            td_api::UserStatus::userStatusOnline(online) if online.expires == i32::MAX
        ));
        for (was_online, expect) in [
            (0, "empty"),
            (-1, "recently"),
            (-4, "recently"),
            (-2, "lastWeek"),
            (-5, "lastWeek"),
            (-3, "lastMonth"),
            (-6, "lastMonth"),
        ] {
            let status = make(&mut um, was_online);
            let name = match &status {
                td_api::UserStatus::userStatusEmpty(_) => "empty",
                td_api::UserStatus::userStatusRecently(_) => "recently",
                td_api::UserStatus::userStatusLastWeek(_) => "lastWeek",
                td_api::UserStatus::userStatusLastMonth(_) => "lastMonth",
                td_api::UserStatus::userStatusOnline(_) => "online",
                td_api::UserStatus::userStatusOffline(_) => "offline",
            };
            assert_eq!(name, expect, "was_online {was_online}");
        }
        // a future timestamp is online, a past one offline
        let unix_time = env.unix_time();
        assert!(matches!(
            make(&mut um, unix_time + 60),
            td_api::UserStatus::userStatusOnline(_)
        ));
        assert!(matches!(
            make(&mut um, unix_time - 60),
            td_api::UserStatus::userStatusOffline(_)
        ));
    }

    #[test]
    fn user_store_value_round_trips() {
        let (mut temp, _um) = test_env();
        let ctx = temp.env(false).time_ctx();
        let mut user = User::new();
        user.first_name = "Alice".to_owned();
        user.last_name = "Smith".to_owned();
        user.phone_number = "+15551234567".to_owned();
        user.access_hash = 42;
        user.is_received = true;
        user.was_online = 123456;
        user.cache_version = USER_CACHE_VERSION;
        user.is_bot = false;
        user.is_deleted = false;
        user.usernames = Usernames::from_telegram("asmith".to_owned(), Vec::new());
        user.emoji_status = EmojiStatus::get_emoji_status(telegram_api::EmojiStatus::emojiStatus(
            telegram_api::emojiStatus {
                flags: 0,
                document_id: 777,
                until: 0,
            },
        ));
        user.restriction_reasons = vec![RestrictionReason::new(
            "all".to_owned(),
            "violence".to_owned(),
            "graphic".to_owned(),
        )];
        user.linked_community_id = CommunityId::new(5);
        user.accent_color_id = AccentColorId::new(2);
        user.profile_accent_color_id = AccentColorId::new(3);
        user.paid_message_star_count = 12;
        user.bot_verification_icon = CustomEmojiId::new(9);
        user.max_active_story_id = StoryId::new(100);
        user.max_active_story_id_next_reload_time = ctx.now + 3600.0;
        user.stories_hidden = true;
        user.is_close_friend = true;
        // the parse validity gate zeroes cache_version for a close friend
        // that is not a contact (UserManager.cpp:2171)
        user.is_contact = true;

        let mut bytes = Vec::new();
        user.store_value(&mut bytes, &ctx);
        let parsed = User::parse_value(&bytes, &ctx).unwrap();
        assert_eq!(parsed.first_name, "Alice");
        assert_eq!(parsed.last_name, "Smith");
        assert_eq!(parsed.phone_number, "+15551234567");
        assert_eq!(parsed.access_hash, 42);
        assert!(parsed.is_received);
        assert_eq!(parsed.was_online, 123456);
        assert_eq!(parsed.cache_version, USER_CACHE_VERSION);
        assert!(!parsed.is_bot);
        assert!(!parsed.is_deleted);
        assert_eq!(parsed.usernames.get_first_username(), "asmith");
        assert_eq!(
            parsed.emoji_status.unwrap().get_custom_emoji_id().get(),
            777
        );
        assert_eq!(parsed.restriction_reasons.len(), 1);
        assert_eq!(parsed.restriction_reasons[0].platform(), "all");
        assert_eq!(parsed.linked_community_id.get(), 5);
        assert_eq!(parsed.accent_color_id.get(), 2);
        assert_eq!(parsed.profile_accent_color_id.get(), 3);
        assert_eq!(parsed.paid_message_star_count, 12);
        assert_eq!(parsed.bot_verification_icon.get(), 9);
        assert_eq!(parsed.max_active_story_id.get(), 100);
        assert!(
            (parsed.max_active_story_id_next_reload_time
                - user.max_active_story_id_next_reload_time)
                .abs()
                < 60.0,
            "store_time/parse_time round-trips through the server-time rebase"
        );
        assert!(parsed.stories_hidden);
        assert!(parsed.is_close_friend);
        assert_eq!(parsed.bot_info_version, -1);
    }

    #[test]
    fn user_parse_rejects_stored_photos() {
        let (mut temp, _um) = test_env();
        let ctx = temp.env(false).time_ctx();
        // A value with the photo flag set cannot be parsed until the Files
        // step lands.
        let mut bytes = Vec::new();
        store_parse::store_log_event_version(&mut bytes);
        let mut flags = [false; 30];
        flags[10] = true; // has_photo
        store_parse::store_flags(&mut bytes, flags);
        assert!(User::parse_value(&bytes, &ctx).is_err());
    }

    #[test]
    fn secret_chat_store_value_round_trips() {
        let mut secret_chat = SecretChat::new();
        secret_chat.access_hash = 999;
        secret_chat.user_id = UserId::new(42);
        secret_chat.state = SecretChatState::Active;
        secret_chat.ttl = 60;
        secret_chat.date = 1700000000;
        secret_chat.key_hash = "hash".to_owned();
        secret_chat.layer = 101;
        secret_chat.is_outbound = true;

        let mut bytes = Vec::new();
        secret_chat.store_value(&mut bytes);
        let parsed = SecretChat::parse_value(&bytes).unwrap();
        assert_eq!(parsed.access_hash, 999);
        assert_eq!(parsed.user_id.get(), 42);
        assert_eq!(parsed.state, SecretChatState::Active);
        assert_eq!(parsed.ttl, 60);
        assert_eq!(parsed.date, 1700000000);
        assert_eq!(parsed.key_hash, "hash");
        assert_eq!(parsed.layer, 101);
        assert!(parsed.is_outbound);
    }

    #[test]
    fn user_full_store_value_round_trips() {
        let (mut temp, _um) = test_env();
        let ctx = temp.env(false).time_ctx();
        let mut user_full = UserFull::new();
        user_full.about = "bio text".to_owned();
        user_full.common_chat_count = 3;
        user_full.birthdate = Birthdate::new(7, 11, 1990);
        user_full.personal_channel_id = 123456;
        user_full.is_blocked = true;
        user_full.voice_messages_forbidden = true;
        user_full.gift_count = 5;
        user_full.charge_paid_message_stars = 25;
        user_full.send_paid_message_stars = 15;
        user_full.linked_community_id = CommunityId::new(8);
        user_full.expires_at = ctx.now + 100.0;

        let mut bytes = Vec::new();
        user_full.store_value(&mut bytes, &ctx);
        let parsed = UserFull::parse_value(&bytes, &ctx).unwrap();
        assert_eq!(parsed.about, "bio text");
        assert_eq!(parsed.common_chat_count, 3);
        assert_eq!(parsed.birthdate.get_year(), 1990);
        assert_eq!(parsed.personal_channel_id, 123456);
        assert!(parsed.is_blocked);
        assert!(parsed.voice_messages_forbidden);
        assert_eq!(parsed.gift_count, 5);
        assert_eq!(parsed.charge_paid_message_stars, 25);
        assert_eq!(parsed.send_paid_message_stars, 15);
        assert_eq!(parsed.linked_community_id.get(), 8);
        assert!(parsed.can_pin_messages, "the C++ default");
        assert!(parsed.is_changed);
        assert!(parsed.need_send_update);
        assert!(parsed.need_save_to_database);
    }

    #[test]
    fn load_my_id_reads_and_migrates_the_legacy_form() {
        let (mut temp, _um) = test_env();
        let env = temp.env(false);
        assert_eq!(
            UserManager::load_my_id(env.global.binlog_pmc()),
            UserId::default()
        );
        env.global.binlog_pmc().set("my_id", "user2202815082");
        assert_eq!(
            UserManager::load_my_id(env.global.binlog_pmc()).get(),
            2202815082
        );
        // migrated to the plain decimal form
        assert_eq!(env.global.binlog_pmc().get("my_id"), "2202815082");
        env.global.binlog_pmc().set("my_id", "42");
        assert_eq!(UserManager::load_my_id(env.global.binlog_pmc()).get(), 42);
        env.global.binlog_pmc().set("my_id", "not-a-number");
        assert_eq!(
            UserManager::load_my_id(env.global.binlog_pmc()),
            UserId::default()
        );
    }

    #[test]
    fn persistence_round_trips_through_the_common_kv() {
        let dir = std::env::temp_dir()
            .join(format!("td-rs-m3-persist-{}", std::process::id()))
            .to_string_lossy()
            .into_owned();
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let value = {
            let opened = tddb::td_db::TdDb::open(tddb::td_db::Parameters {
                database_directory: dir.clone(),
                use_file_database: true,
                use_chat_info_database: true,
                ..Default::default()
            })
            .unwrap();
            let mut global = Global::new();
            global.init(opened.database);
            // keep the sqlite file across reopen (authorized database)
            global.binlog_pmc().set("auth", "ok");
            let ctx = TimeCtx::from_global(&global);
            let mut user = User::new();
            user.first_name = "Persist".to_owned();
            user.is_received = true;
            user.is_bot = false;
            user.is_deleted = false;
            user.access_hash = 7;
            let mut bytes = Vec::new();
            user.store_value(&mut bytes, &ctx);
            global.td_db().common_kv().set(b"us5", &bytes);
            global.close_all(false);
            bytes
        };

        // Reopen and read through the manager's load path.
        let opened = tddb::td_db::TdDb::open(tddb::td_db::Parameters {
            database_directory: dir.clone(),
            use_file_database: true,
            use_chat_info_database: true,
            ..Default::default()
        })
        .unwrap();
        let mut global = Global::new();
        global.init(opened.database);
        let loaded = global.td_db().common_kv().get(b"us5");
        assert_eq!(loaded, value);
        let ctx = TimeCtx::from_global(&global);
        let user = User::parse_value(&loaded, &ctx).unwrap();
        assert_eq!(user.first_name, "Persist");
        assert!(user.is_received);
        assert_eq!(user.access_hash, 7);
        global.close_all(false);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The M.4.5 crash-window arc: a `UserLogEvent` left in the binlog
    /// (the C++ state between `binlog_add` and the async sqlite flush)
    /// replays on reopen, re-saves to the "common" kv, and retires its
    /// binlog mirror — and the reopened database stops offering the event.
    #[test]
    fn user_log_event_replays_across_reopen() {
        let dir = std::env::temp_dir()
            .join(format!(
                "td-rs-m4-userlogevent-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ))
            .to_string_lossy()
            .into_owned();
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let user_id = UserId::new(5);

        // Phase 1: simulate the crash window — the binlog mirror was
        // written, the sqlite row never landed.
        {
            let opened = tddb::td_db::TdDb::open(tddb::td_db::Parameters {
                database_directory: dir.clone(),
                use_file_database: true,
                use_chat_info_database: true,
                ..Default::default()
            })
            .unwrap();
            let mut global = Global::new();
            global.init(opened.database);
            // authorized marker travels in the binlog pmc — later reopens
            // must not destroy the sqlite file
            global.binlog_pmc().set("auth", "ok");
            let options = OptionManager::new(global.is_test_dc());
            let ctx = TimeCtx::from_global(&global);
            let mut user = User::new();
            user.first_name = "Crash".to_owned();
            user.is_received = true;
            user.is_bot = false;
            user.is_deleted = false;
            user.access_hash = 11;
            let mut data = Vec::new();
            store_user_log_event(&mut data, user_id, &user, &ctx);
            global
                .td_db()
                .binlog()
                .add(tddb::td_db::LOG_EVENT_HANDLER_TYPE_USERS, &data);
            // no sqlite write, straight to close — the crash
            drop(options);
            global.close_all(false);
        }

        // Phase 2: reopen — TdDb collected the Users event; replay restores
        // the user, saves the sqlite row and erases the mirror.
        {
            let opened = tddb::td_db::TdDb::open(tddb::td_db::Parameters {
                database_directory: dir.clone(),
                use_file_database: true,
                use_chat_info_database: true,
                ..Default::default()
            })
            .unwrap();
            assert_eq!(opened.user_events.len(), 1, "the Users event replays");
            let event = opened.user_events[0].clone();

            let mut global = Global::new();
            global.init(opened.database);
            let mut options = OptionManager::new(global.is_test_dc());
            let mut um = UserManager::new();
            {
                let mut env = UserEnv {
                    global: &mut global,
                    options: &mut options,
                    auth_is_bot: false,
                };
                um.on_binlog_user_event(&mut env, &event);
            }
            let u = um.get_user(user_id).expect("replayed user");
            assert_eq!(u.first_name, "Crash");
            assert!(u.is_received);
            assert_eq!(u.access_hash, 11);
            assert_eq!(u.log_event_id, 0, "the save retired the mirror");

            let row = global.td_db().common_kv().get(b"us5");
            assert!(!row.is_empty(), "the replay saved the sqlite row");
            let ctx = TimeCtx::from_global(&global);
            let parsed = User::parse_value(&row, &ctx).unwrap();
            assert_eq!(parsed.first_name, "Crash");
            global.close_all(false);
        }

        // Phase 3: reopen again — no Users events left; the row loads
        // through the manager's database path.
        {
            let opened = tddb::td_db::TdDb::open(tddb::td_db::Parameters {
                database_directory: dir.clone(),
                use_file_database: true,
                use_chat_info_database: true,
                ..Default::default()
            })
            .unwrap();
            assert!(opened.user_events.is_empty(), "the mirror was retired");
            let mut global = Global::new();
            global.init(opened.database);
            let mut options = OptionManager::new(global.is_test_dc());
            let mut um = UserManager::new();
            assert!(um.get_user_force(
                &mut UserEnv {
                    global: &mut global,
                    options: &mut options,
                    auth_is_bot: false,
                },
                user_id,
                "test"
            ));
            assert_eq!(um.get_user(user_id).unwrap().first_name, "Crash");
            global.close_all(false);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn special_users_are_synthesized() {
        let (mut temp, mut um) = test_env();
        let service = UserManager::get_service_notifications_user_id();
        assert!(um.get_user_force(&mut temp.env(false), service, "test"));
        let u = um.get_user(service).unwrap();
        assert_eq!(u.first_name, "Telegram");
        assert!(u.is_verified);
        assert!(u.is_support);
        assert_eq!(u.phone_number, "42777");
        assert!(u.is_received);

        let replies = UserManager::get_replies_bot_user_id(temp.global.is_test_dc());
        assert!(um.get_user_force(&mut temp.env(false), replies, "test"));
        let u = um.get_user(replies).unwrap();
        assert!(u.is_bot);
        assert_eq!(u.usernames.get_first_username(), "replies");

        // get_user_local answers for special users
        assert!(um.get_user_local(&mut temp.env(false), service));
        assert!(!um.get_user_local(&mut temp.env(false), UserId::new(12345)));
    }

    #[test]
    fn get_me_requires_the_forced_user() {
        let (mut temp, mut um) = test_env();
        assert_eq!(um.get_me(&mut temp.env(false)), UserId::default());
        // authorize the synthetic self
        um.my_id = UserId::new(77);
        let mut me = telegram_user(77, "Me", USER_ACCESS_HASH_MASK | (1 << 1));
        me.self_ = true;
        um.on_get_user(&mut temp.env(false), telegram_api::User::user(me), "t");
        assert_eq!(um.get_me(&mut temp.env(false)).get(), 77);
    }

    #[test]
    fn update_user_emits_status_and_online_deadline() {
        let (mut temp, mut um) = test_env();
        // A near-future expiry lands inside the 30-day deadline window
        // (update_user drops the deadline otherwise). The Telegram status
        // timestamps are unix time — `time::system()`, not the
        // process-anchored monotonic clock.
        let expires = (tdutils::time::system() as i64 + 3600) as i32;
        let full_flags = USER_ACCESS_HASH_MASK | (1 << 1) | (1 << 6); // access_hash, first_name, status
        let mut user = telegram_user(5, "Bob", full_flags);
        user.status =
            telegram_api::UserStatus::userStatusOnline(telegram_api::userStatusOnline { expires });
        um.on_get_user(&mut temp.env(false), telegram_api::User::user(user), "t");
        let updates = um.take_updates();
        // A new user folds the status into its updateUser — a separate
        // updateUserStatus fires only for a status-only change (the C++
        // clears is_status_changed inside the is_changed block).
        assert!(
            updates
                .iter()
                .any(|update| matches!(update, td_api::Update::updateUser(u) if u.user.id == 5)),
            "the new user must be sent"
        );
        assert!(
            um.user_online_timeouts.contains_key(&5),
            "the online-expiry deadline must be scheduled"
        );

        // A status-only change emits updateUserStatus.
        um.on_update_user_online(
            &mut temp.env(false),
            UserId::new(5),
            telegram_api::UserStatus::userStatusOffline(telegram_api::userStatusOffline {
                was_online: tdutils::time::system() as i32 - 120,
            }),
        );
        let updates = um.take_updates();
        assert!(
            updates.iter().any(|update| matches!(
                update,
                td_api::Update::updateUserStatus(s) if s.user_id == 5
            )),
            "the status-only change must be sent"
        );
    }

    #[test]
    fn on_user_online_timeout_flips_to_offline_object() {
        let (mut temp, mut um) = test_env();
        let expires = (tdutils::time::system() as i64 + 60) as i32;
        let full_flags = USER_ACCESS_HASH_MASK | (1 << 1) | (1 << 6); // access_hash, first_name, status
        let mut user = telegram_user(5, "Bob", full_flags);
        user.status =
            telegram_api::UserStatus::userStatusOnline(telegram_api::userStatusOnline { expires });
        um.on_get_user(&mut temp.env(false), telegram_api::User::user(user), "t");
        um.take_updates();
        // move was_online into the past, then fire the deadline
        um.users.get_mut(&5).unwrap().was_online -= 3600;
        let env = &mut temp.env(false);
        um.user_online_timeouts
            .insert(5, tdutils::time::now() - 1.0);
        um.tick_timeouts(env);
        let updates = um.take_updates();
        assert!(updates.iter().any(|update| matches!(
            update,
            td_api::Update::updateUserStatus(s) if s.user_id == 5 &&
                matches!(s.status, td_api::UserStatus::userStatusOffline(_))
        )));
    }

    #[test]
    fn status_object_covers_the_was_online_bands() {
        let (mut temp, mut um) = test_env();
        let env = &mut temp.env(false);

        // bots report userStatusOnline forever
        let mut bot = telegram_user(1, "Bot", USER_ACCESS_HASH_MASK | (1 << 1));
        bot.bot = true;
        um.on_get_user(env, telegram_api::User::user(bot), "t");
        assert!(matches!(
            um.get_user_status_object(env, UserId::new(1)),
            td_api::UserStatus::userStatusOnline(s) if s.expires == i32::MAX
        ));

        um.on_get_user(
            env,
            telegram_api::User::user(telegram_user(2, "Band", USER_ACCESS_HASH_MASK | (1 << 1))),
            "t",
        );
        um.take_updates();
        let now = env.unix_time();
        let check = |um: &mut UserManager, was_online: i32| {
            um.users.get_mut(&2).unwrap().was_online = was_online;
            um.get_user_status_object(env, UserId::new(2))
        };
        assert!(matches!(
            check(&mut um, -6),
            td_api::UserStatus::userStatusLastMonth(s) if s.by_my_privacy_settings
        ));
        assert!(matches!(
            check(&mut um, -3),
            td_api::UserStatus::userStatusLastMonth(s) if !s.by_my_privacy_settings
        ));
        assert!(matches!(
            check(&mut um, -5),
            td_api::UserStatus::userStatusLastWeek(s) if s.by_my_privacy_settings
        ));
        assert!(matches!(
            check(&mut um, -2),
            td_api::UserStatus::userStatusLastWeek(s) if !s.by_my_privacy_settings
        ));
        assert!(matches!(
            check(&mut um, -4),
            td_api::UserStatus::userStatusRecently(s) if s.by_my_privacy_settings
        ));
        assert!(matches!(
            check(&mut um, -1),
            td_api::UserStatus::userStatusRecently(s) if !s.by_my_privacy_settings
        ));
        assert!(matches!(
            check(&mut um, 0),
            td_api::UserStatus::userStatusEmpty(_)
        ));
        assert!(matches!(
            check(&mut um, now + 300),
            td_api::UserStatus::userStatusOnline(s) if s.expires == now + 300
        ));
        assert!(matches!(
            check(&mut um, now - 300),
            td_api::UserStatus::userStatusOffline(s) if s.was_online == now - 300
        ));
    }

    #[test]
    fn is_user_online_respects_the_tolerance() {
        let (mut temp, mut um) = test_env();
        let env = &mut temp.env(false);
        um.on_get_user(
            env,
            telegram_api::User::user(telegram_user(3, "Tol", USER_ACCESS_HASH_MASK | (1 << 1))),
            "t",
        );
        um.take_updates();
        let now = env.unix_time();
        um.users.get_mut(&3).unwrap().was_online = now - 30;
        assert!(!um.is_user_online_of(env, UserId::new(3), 0, 0));
        assert!(!um.is_user_online_of(env, UserId::new(3), 30, 0)); // the boundary is strict
        assert!(um.is_user_online_of(env, UserId::new(3), 31, 0));
        // a user still online ignores the tolerance
        um.users.get_mut(&3).unwrap().was_online = now + 60;
        assert!(um.is_user_online_of(env, UserId::new(3), 0, 0));
        // an explicit unix_time overrides now
        um.users.get_mut(&3).unwrap().was_online = 1000;
        assert!(!um.is_user_online_of(env, UserId::new(3), 0, 2000));
    }

    #[test]
    fn get_user_was_online_pairs_the_local_overrides() {
        let (mut temp, mut um) = test_env();
        let env = &mut temp.env(false);
        um.my_id = UserId::new(7);
        let mut me = telegram_user(7, "Me", USER_ACCESS_HASH_MASK | (1 << 1));
        me.self_ = true;
        um.on_get_user(env, telegram_api::User::user(me), "t");
        um.take_updates();
        let now = env.unix_time();

        // self: the my_was_online_local override wins
        um.users.get_mut(&7).unwrap().was_online = now - 300;
        assert_eq!(um.get_user_was_online_of(UserId::new(7), now), now - 300);
        um.my_was_online_local = now - 30;
        assert_eq!(um.get_user_was_online_of(UserId::new(7), now), now - 30);
        um.my_was_online_local = 0;

        // others: local_was_online overrides only when larger, positive and
        // still in the future
        um.on_get_user(
            env,
            telegram_api::User::user(telegram_user(8, "Other", USER_ACCESS_HASH_MASK | (1 << 1))),
            "t",
        );
        um.take_updates();
        um.users.get_mut(&8).unwrap().was_online = now - 300;
        assert_eq!(um.get_user_was_online_of(UserId::new(8), now), now - 300);
        um.users.get_mut(&8).unwrap().local_was_online = now - 100; // past: ignored
        assert_eq!(um.get_user_was_online_of(UserId::new(8), now), now - 300);
        um.users.get_mut(&8).unwrap().local_was_online = now + 60; // future: wins
        assert_eq!(um.get_user_was_online_of(UserId::new(8), now), now + 60);
        um.users.get_mut(&8).unwrap().was_online = now + 120; // remote online beats local
        assert_eq!(um.get_user_was_online_of(UserId::new(8), now), now + 120);
    }

    #[test]
    fn is_user_status_exact_rejects_bots_deleted_and_unset() {
        let (mut temp, mut um) = test_env();
        let env = &mut temp.env(false);
        assert!(!um.is_user_status_exact_of(UserId::new(9))); // unknown

        um.on_get_user(
            env,
            telegram_api::User::user(telegram_user(9, "Exact", USER_ACCESS_HASH_MASK | (1 << 1))),
            "t",
        );
        um.take_updates();
        assert!(!um.is_user_status_exact_of(UserId::new(9))); // was_online == 0
        um.users.get_mut(&9).unwrap().was_online = env.unix_time() - 60;
        assert!(um.is_user_status_exact_of(UserId::new(9)));

        let mut bot = telegram_user(10, "Bot", USER_ACCESS_HASH_MASK | (1 << 1));
        bot.bot = true;
        um.on_get_user(env, telegram_api::User::user(bot), "t");
        um.take_updates();
        um.users.get_mut(&10).unwrap().was_online = env.unix_time() - 60;
        assert!(!um.is_user_status_exact_of(UserId::new(10))); // bots never exact

        um.users.get_mut(&9).unwrap().is_deleted = true;
        assert!(!um.is_user_status_exact_of(UserId::new(9))); // deleted never exact
    }

    #[test]
    fn my_online_status_pmc_keys_round_trip() {
        let (mut temp, mut um) = test_env();
        um.my_id = UserId::new(7);
        let unix_time = {
            let env = &mut temp.env(false);
            let mut me = telegram_user(7, "Me", USER_ACCESS_HASH_MASK | (1 << 1));
            me.self_ = true;
            um.on_get_user(env, telegram_api::User::user(me), "t");
            um.take_updates();
            env.unix_time()
        };

        // the local offline clamps to the stored was_online and persists
        let past = unix_time - 500;
        um.users.get_mut(&7).unwrap().was_online = past;
        um.set_my_online_status(&mut temp.env(false), false, false, true);
        assert_eq!(um.my_was_online_local, past);
        assert_eq!(
            temp.global.binlog_pmc().get("my_was_online_local"),
            past.to_string()
        );

        // the local online persists unix_time + 300
        um.set_my_online_status(&mut temp.env(false), true, false, true);
        assert_eq!(um.my_was_online_local, unix_time + 300);

        // the remote path clears the local override and stores was_online
        um.set_my_online_status(&mut temp.env(false), false, false, false);
        assert_eq!(um.my_was_online_local, 0);
        assert_eq!(um.users.get(&7).unwrap().was_online, unix_time - 1);
        assert_eq!(
            temp.global.binlog_pmc().get("my_was_online_local"),
            (unix_time - 1).to_string()
        );

        // an updateUserStatus for self persists my_was_online_remote
        let remote = unix_time - 500;
        um.on_update_user_online(
            &mut temp.env(false),
            UserId::new(7),
            telegram_api::UserStatus::userStatusOffline(telegram_api::userStatusOffline {
                was_online: remote,
            }),
        );
        assert_eq!(
            temp.global.binlog_pmc().get("my_was_online_remote"),
            remote.to_string()
        );

        // a fresh manager restores both keys from the same binlog pmc
        let mut restored = UserManager::new();
        restored.init_my_online_status(temp.global.binlog_pmc(), unix_time, false);
        assert_eq!(restored.was_online_local, um.was_online_local);
        assert_eq!(restored.was_online_remote, remote);

        // a stored future local time is clamped back when we are offline
        temp.global
            .binlog_pmc()
            .set("my_was_online_local", &(unix_time + 60).to_string());
        let mut clamped = UserManager::new();
        clamped.init_my_online_status(temp.global.binlog_pmc(), unix_time, false);
        assert_eq!(clamped.was_online_local, unix_time - 1);
        // ... and kept when the OnlineManager says we are online
        let mut online = UserManager::new();
        online.init_my_online_status(temp.global.binlog_pmc(), unix_time, true);
        assert_eq!(online.was_online_local, unix_time + 60);
    }

    #[test]
    fn on_get_user_full_applies_the_supported_fields() {
        let (mut temp, mut um) = test_env();
        // the user must exist first (the C++ get_user_force)
        let full_flags = USER_ACCESS_HASH_MASK | (1 << 1); // access_hash, first_name
        um.on_get_user(
            &mut temp.env(false),
            telegram_api::User::user(telegram_user(5, "A", full_flags)),
            "t",
        );
        um.take_updates();

        let mut user_full = telegram_api::UserFull {
            id: 5,
            flags: (1 << 1) | (1 << 4), // about, phone_calls_available
            common_chats_count: 3,
            stargifts_count: 7,
            ..telegram_api::UserFull::default()
        };
        user_full.about = "the bio".to_owned();
        um.on_get_user_full(&mut temp.env(false), user_full);
        let stored = um.get_user_full(UserId::new(5)).unwrap();
        assert_eq!(stored.about, "the bio");
        assert_eq!(stored.common_chat_count, 3);
        assert_eq!(stored.gift_count, 7);
        assert!(stored.can_be_called);
        assert!(!stored.is_expired(&TimeCtx {
            now: tdutils::time::now(),
            server_time: 0.0
        }));
        let updates = um.take_updates();
        assert!(updates.iter().any(|update| matches!(
            update,
            td_api::Update::updateUserFullInfo(f) if f.user_id == 5 && f.user_full_info.bio.text == "the bio"
        )));
    }

    #[test]
    fn get_current_state_streams_known_users() {
        let (mut temp, mut um) = test_env();
        let full_flags = USER_ACCESS_HASH_MASK | (1 << 1); // access_hash, first_name
        um.on_get_user(
            &mut temp.env(false),
            telegram_api::User::user(telegram_user(5, "A", full_flags)),
            "t",
        );
        let mut updates = Vec::new();
        um.get_current_state(&mut temp.env(false), &mut updates);
        assert!(updates.iter().any(|update| matches!(
            update,
            td_api::Update::updateUser(u) if u.user.id == 5
        )));
    }

    // --- M.4.6 verification vectors (telegram_api::user → store mutations +
    // --- emitted updates), per RUST_REWRITE_PLAN_USERMANAGER_M46.md §3.

    #[test]
    fn vector_user_empty_returns_the_id_without_touching_the_cache() {
        let (mut temp, mut um) = test_env();
        let user_id = um.on_get_user(
            &mut temp.env(false),
            telegram_api::User::userEmpty(telegram_api::userEmpty { id: 5 }),
            "t",
        );
        assert_eq!(user_id.get(), 5);
        assert!(um.get_user(UserId::new(5)).is_none());
    }

    #[test]
    fn vector_deleted_user_is_fully_sanitized() {
        // C++ :3286-3310 resets the premium/support/min-photo/star family
        // too, not only the bot-flag family.
        let (mut temp, mut um) = test_env();
        let mut user = telegram_user(5, "Ghost", USER_ACCESS_HASH_MASK | (1 << 1));
        user.deleted = true;
        user.premium = true;
        user.support = true;
        user.apply_min_photo = true;
        user.send_paid_messages_stars = 100;
        user.bot_chat_history = true;
        um.on_get_user(&mut temp.env(false), telegram_api::User::user(user), "t");
        let u = um.get_user(UserId::new(5)).unwrap();
        assert!(u.is_deleted);
        assert!(!u.is_premium);
        assert!(!u.is_support);
        assert!(!u.need_apply_min_photo);
        assert_eq!(u.paid_message_star_count, 0);
        assert!(
            !u.can_read_all_group_messages,
            "non-bot bot props are reset"
        );
    }

    #[test]
    fn vector_unknown_users_survives_a_cached_path_delivery() {
        // C++ erases from unknown_users_ only on the add-user path
        // (UserManager.cpp:3196-3199); an already-cached delivery must not.
        let (mut temp, mut um) = test_env();
        let full_flags = USER_ACCESS_HASH_MASK | (1 << 1);
        um.on_get_user(
            &mut temp.env(false),
            telegram_api::User::user(telegram_user(5, "A", full_flags)),
            "t",
        );
        um.unknown_users.insert(5);
        um.on_get_user(
            &mut temp.env(false),
            telegram_api::User::user(telegram_user(5, "A", full_flags)),
            "t",
        );
        assert!(
            um.unknown_users.contains(&5),
            "cached-path delivery must not erase unknown_users"
        );
    }

    #[test]
    fn vector_username_flag_is_consumed_by_update_user() {
        // C++ clears is_username_changed in update_user where
        // update_contacts_hints runs (UserManager.cpp:9889-9892).
        let (mut temp, mut um) = test_env();
        let mut user = telegram_user(5, "Alice", USER_ACCESS_HASH_MASK | (1 << 1) | (1 << 3)); // username
        user.username = "alice".to_owned();
        um.on_get_user(&mut temp.env(false), telegram_api::User::user(user), "t");
        assert!(!um.get_user(UserId::new(5)).unwrap().usernames.is_empty());
        assert!(!um.get_user(UserId::new(5)).unwrap().is_username_changed);
    }

    #[test]
    fn vector_non_bot_with_bot_properties_is_sanitized() {
        let (mut temp, mut um) = test_env();
        let mut user = telegram_user(5, "Bot", USER_ACCESS_HASH_MASK | (1 << 1));
        user.bot_chat_history = true; // can_read_all_group_messages
        user.bot_inline_geo = true; // need_location_bot without inline bot
        um.on_get_user(&mut temp.env(false), telegram_api::User::user(user), "t");
        let u = um.get_user(UserId::new(5)).unwrap();
        assert!(!u.can_read_all_group_messages);
        assert!(!u.need_location_bot);
    }

    #[test]
    fn vector_self_bot_active_users_is_throttled() {
        let (mut temp, mut um) = test_env();
        let mut user = telegram_user(77, "Me", USER_ACCESS_HASH_MASK | (1 << 1));
        user.self_ = true;
        user.bot_active_users = 10;
        um.on_get_user(&mut temp.env(false), telegram_api::User::user(user), "t");
        assert_eq!(um.get_user(UserId::new(77)).unwrap().bot_active_users, 10);
        // A second delivery within the 600 s window is dropped.
        let mut user = telegram_user(77, "Me", USER_ACCESS_HASH_MASK | (1 << 1));
        user.self_ = true;
        user.bot_active_users = 99;
        um.on_get_user(&mut temp.env(false), telegram_api::User::user(user), "t");
        assert_eq!(um.get_user(UserId::new(77)).unwrap().bot_active_users, 10);
    }

    #[test]
    fn vector_first_delivery_swallows_status_into_update_user() {
        let (mut temp, mut um) = test_env();
        // Status expires live in the env's unix-time base (server time);
        // the process-monotonic clock would read as an ancient status.
        let expires = temp.env(false).unix_time() + 300;
        let mut user = telegram_user(5, "A", USER_ACCESS_HASH_MASK | (1 << 1));
        user.status =
            telegram_api::UserStatus::userStatusOnline(telegram_api::userStatusOnline { expires });
        um.on_get_user(&mut temp.env(false), telegram_api::User::user(user), "t");
        let updates = um.take_updates();
        assert!(
            updates
                .iter()
                .any(|u| matches!(u, td_api::Update::updateUser(x) if x.user.id == 5)),
            "updateUser must be sent"
        );
        // The C++ diff machine clears is_status_changed together with the
        // updateUser send (UserManager.cpp:10027-10033): a first delivery
        // never emits a standalone updateUserStatus.
        assert!(
            updates
                .iter()
                .all(|u| !matches!(u, td_api::Update::updateUserStatus(_))),
            "status must ride the updateUser, not a separate update"
        );
        // The online deadline is armed for the non-self user.
        assert!(um.user_online_timeouts.contains_key(&5));
    }
}
